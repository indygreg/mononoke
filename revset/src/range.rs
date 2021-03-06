// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::collections::hash_set::IntoIter;
use std::iter;
use std::mem::replace;
use std::sync::Arc;

use futures::{Async, Poll};
use futures::future::Future;
use futures::stream::{self, iter_ok, Stream};

use mercurial_types::{NodeHash, Repo};
use repoinfo::{Generation, RepoGenCache};

use NodeStream;
use errors::*;

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HashGen {
    hash: NodeHash,
    generation: Generation,
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct ParentChild {
    parent: HashGen,
    child: HashGen,
}

pub struct RangeNodeStream<R>
where
    R: Repo,
{
    repo: Arc<R>,
    repo_generation: RepoGenCache<R>,
    start_node: NodeHash,
    start_generation: Box<Stream<Item = Generation, Error = Error> + Send>,
    children: HashMap<HashGen, HashSet<HashGen>>,
    // Child, parent
    pending_nodes: Box<Stream<Item = ParentChild, Error = Error> + Send>,
    output_nodes: Option<BTreeMap<Generation, HashSet<NodeHash>>>,
    drain: Option<IntoIter<NodeHash>>,
}

fn make_pending<R: Repo>(
    repo: Arc<R>,
    repo_generation: RepoGenCache<R>,
    child: HashGen,
) -> Box<Stream<Item = ParentChild, Error = Error> + Send> {
    Box::new(
        {
            let repo = repo.clone();
            repo.get_changeset_by_nodeid(&child.hash)
                .map(move |cs| (child, cs.parents().clone()))
                .map_err(|err| Error::with_chain(err, ErrorKind::ParentsFetchFailed))
        }.map(|(child, parents)| {
            iter_ok::<_, Error>(iter::repeat(child).zip(parents.into_iter()))
        })
            .flatten_stream()
            .and_then(move |(child, parent_hash)| {
                repo_generation
                    .get(&repo, parent_hash)
                    .map(move |gen_id| {
                        ParentChild {
                            child,
                            parent: HashGen {
                                hash: parent_hash,
                                generation: gen_id,
                            },
                        }
                    })
                    .map_err(|err| {
                        Error::with_chain(err, ErrorKind::GenerationFetchFailed)
                    })
            }),
    )
}

impl<R> RangeNodeStream<R>
where
    R: Repo,
{
    pub fn new(
        repo: &Arc<R>,
        repo_generation: RepoGenCache<R>,
        start_node: NodeHash,
        end_node: NodeHash,
    ) -> Self {
        let start_generation = Box::new(
            repo_generation
                .get(repo, start_node)
                .map_err(|err| {
                    Error::with_chain(err, ErrorKind::GenerationFetchFailed)
                })
                .map(stream::repeat)
                .flatten_stream(),
        );

        let pending_nodes = {
            let repo = repo.clone();
            let repo_generation = repo_generation.clone();
            Box::new(
                repo_generation
                    .get(&repo, end_node)
                    .map_err(|err| {
                        Error::with_chain(err, ErrorKind::GenerationFetchFailed)
                    })
                    .map(move |generation| {
                        make_pending(
                            repo,
                            repo_generation,
                            HashGen {
                                hash: end_node,
                                generation,
                            },
                        )
                    })
                    .flatten_stream(),
            )
        };

        RangeNodeStream {
            repo: repo.clone(),
            repo_generation,
            start_node,
            start_generation,
            children: HashMap::new(),
            pending_nodes,
            output_nodes: None,
            drain: None,
        }
    }

    pub fn boxed(self) -> Box<NodeStream> {
        Box::new(self)
    }

    fn build_output_nodes(&mut self, start_generation: Generation) {
        // We've been walking backwards from the end point, storing the nodes we see.
        // Now walk forward from the start point, looking at children only. These are
        // implicitly the range we want, because we only have children reachable from
        // the end point
        let mut output_nodes = BTreeMap::new();
        let mut nodes_to_handle = HashSet::new();
        // If this is empty, then the end node came before the start node
        if !self.children.is_empty() {
            nodes_to_handle.insert(HashGen {
                hash: self.start_node,
                generation: start_generation,
            });
        }
        while !nodes_to_handle.is_empty() {
            let nodes = replace(&mut nodes_to_handle, HashSet::new());
            for hashgen in nodes {
                output_nodes
                    .entry(hashgen.generation)
                    .or_insert_with(HashSet::new)
                    .insert(hashgen.hash);
                if let Some(children) = self.children.get(&hashgen) {
                    nodes_to_handle.extend(children);
                }
            }
        }
        self.output_nodes = Some(output_nodes);
    }
}

impl<R> Stream for RangeNodeStream<R>
where
    R: Repo,
{
    type Item = NodeHash;
    type Error = Error;
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Empty the drain; this can only happen once we're in Stage 2
        let next_in_drain = self.drain.as_mut().and_then(|drain| drain.next());
        if next_in_drain.is_some() {
            return Ok(Async::Ready(next_in_drain));
        }

        // Stage 1 - no output nodes yet, so let's build it.
        if self.output_nodes.is_none() {
            loop {
                let start_generation = self.start_generation.poll()?;
                let start_generation = if let Async::Ready(Some(x)) = start_generation {
                    x
                } else {
                    return Ok(Async::NotReady);
                };

                let next_pending = self.pending_nodes.poll()?;
                let next_pending = match next_pending {
                    Async::Ready(None) => {
                        self.build_output_nodes(start_generation);
                        break;
                    }
                    Async::Ready(Some(x)) => x,
                    Async::NotReady => return Ok(Async::NotReady),
                };

                if next_pending.child.generation >= start_generation {
                    self.children
                        .entry(next_pending.parent)
                        .or_insert_with(HashSet::new)
                        .insert(next_pending.child);
                }

                if next_pending.parent.generation > start_generation {
                    let old_pending = replace(&mut self.pending_nodes, Box::new(stream::empty()));
                    let pending = old_pending.chain(make_pending(
                        self.repo.clone(),
                        self.repo_generation.clone(),
                        next_pending.parent,
                    ));
                    self.pending_nodes = Box::new(pending);
                }
            }
        }

        // If we get here, we're in Stage 2 (and will never re-enter Stage 1).
        // Convert the tree of children that are ancestors of the end node into
        // the drain to output
        match self.output_nodes {
            Some(ref mut nodes) => if !nodes.is_empty() {
                let highest_generation = *nodes.keys().max().expect("Non-empty map has no keys");
                let current_generation = nodes
                    .remove(&highest_generation)
                    .expect("Highest generation doesn't exist");
                self.drain = Some(current_generation.into_iter());
                return Ok(Async::Ready(Some(
                    self.drain
                        .as_mut()
                        .and_then(|drain| drain.next())
                        .expect("Cannot create a generation without at least one node hash"),
                )));
            },
            _ => panic!("No output_nodes"),
        }
        Ok(Async::Ready(None))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use linear;
    use merge_uneven;
    use tests::assert_node_sequence;
    use tests::string_to_nodehash;

    #[test]
    fn linear_range() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
        ).boxed();

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
                string_to_nodehash("0ed509bf086fadcb8a8a5384dc3b550729b0fc17"),
                string_to_nodehash("eed3a8c0ec67b6a6fe2eb3543334df3f0b4f202b"),
                string_to_nodehash("cb15ca4a43a59acff5388cea9648c162afde8372"),
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            ],
            nodestream,
        )
    }

    #[test]
    fn linear_direct_parent_range() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("0ed509bf086fadcb8a8a5384dc3b550729b0fc17"),
            string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
        ).boxed();

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
                string_to_nodehash("0ed509bf086fadcb8a8a5384dc3b550729b0fc17"),
            ],
            nodestream,
        )
    }

    #[test]
    fn linear_single_node_range() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
        ).boxed();

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
            ],
            nodestream,
        )
    }

    #[test]
    fn linear_empty_range() {
        let repo = Arc::new(linear::getrepo());
        let repo_generation = RepoGenCache::new(10);

        // These are swapped, so won't find anything
        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("a9473beb2eb03ddb1cccc3fbaeb8a4820f9cd157"),
            string_to_nodehash("d0a361e9022d226ae52f689667bd7d212a19cfe0"),
        ).boxed();

        assert_node_sequence(repo_generation, &repo, vec![], nodestream)
    }

    #[test]
    fn merge_range_from_merge() {
        let repo = Arc::new(merge_uneven::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("1d8a907f7b4bf50c6a09c16361e2205047ecc5e5"),
            string_to_nodehash("75742e6fc286a359b39a89fdfa437cc7e2a0e1ce"),
        ).boxed();

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("75742e6fc286a359b39a89fdfa437cc7e2a0e1ce"),
                string_to_nodehash("16839021e338500b3cf7c9b871c8a07351697d68"),
                string_to_nodehash("1d8a907f7b4bf50c6a09c16361e2205047ecc5e5"),
            ],
            nodestream,
        )
    }

    #[test]
    fn merge_range_everything() {
        let repo = Arc::new(merge_uneven::getrepo());
        let repo_generation = RepoGenCache::new(10);

        let nodestream = RangeNodeStream::new(
            &repo,
            repo_generation.clone(),
            string_to_nodehash("15c40d0abc36d47fb51c8eaec51ac7aad31f669c"),
            string_to_nodehash("75742e6fc286a359b39a89fdfa437cc7e2a0e1ce"),
        ).boxed();

        assert_node_sequence(
            repo_generation,
            &repo,
            vec![
                string_to_nodehash("75742e6fc286a359b39a89fdfa437cc7e2a0e1ce"),
                string_to_nodehash("264f01429683b3dd8042cb3979e8bf37007118bc"),
                string_to_nodehash("5d43888a3c972fe68c224f93d41b30e9f888df7c"),
                string_to_nodehash("fc2cef43395ff3a7b28159007f63d6529d2f41ca"),
                string_to_nodehash("bc7b4d0f858c19e2474b03e442b8495fd7aeef33"),
                string_to_nodehash("795b8133cf375f6d68d27c6c23db24cd5d0cd00f"),
                string_to_nodehash("4f7f3fd428bec1a48f9314414b063c706d9c1aed"),
                string_to_nodehash("16839021e338500b3cf7c9b871c8a07351697d68"),
                string_to_nodehash("1d8a907f7b4bf50c6a09c16361e2205047ecc5e5"),
                string_to_nodehash("b65231269f651cfe784fd1d97ef02a049a37b8a0"),
                string_to_nodehash("d7542c9db7f4c77dab4b315edd328edf1514952f"),
                string_to_nodehash("3cda5c78aa35f0f5b09780d971197b51cad4613a"),
                string_to_nodehash("15c40d0abc36d47fb51c8eaec51ac7aad31f669c"),
            ],
            nodestream,
        )
    }
}
