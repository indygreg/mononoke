// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

use mercurial_types::NodeHash;

error_chain! {
    errors {
        RepoError(hash: NodeHash) {
            description("repo error")
            display("repo error checking for node: {}", hash)
        }
        GenerationFetchFailed {
            description("could not fetch node generation")
        }
        ParentsFetchFailed {
            description("failed to fetch parent nodes")
        }
    }
}
