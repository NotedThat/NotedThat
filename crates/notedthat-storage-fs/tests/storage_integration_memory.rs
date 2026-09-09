#![allow(missing_docs)]

//! The storage integration suite, run against the in-memory substitute.
//!
//! Every test body lives in `support/integration_scenarios.rs` and is shared with
//! `storage_integration_local.rs` and `storage_integration_s3.rs`. This file contributes
//! only the fixture: an `InMemoryStorage` of its own per scenario.
//!
//! `InMemoryStorage` is not a real backend, so why hold it to a real backend's suite?
//! Because it is what almost every E2E suite in the workspace runs on, and
//! `DEVELOPMENT.md` warns that a green E2E run therefore says nothing about production
//! behaviour. `storage_conformance_local.rs` already checks that it *agrees* with
//! `FsStorage`; this checks the other half — that what they agree on is right.
//!
//! Run with: `cargo test -p notedthat-storage-fs --test storage_integration_memory`

#[path = "support/integration_scenarios.rs"]
mod scenarios;

use notedthat_core::{KbSlug, testing::InMemoryStorage};
use scenarios::{kb_for, storage_integration_scenarios};

struct Fixture {
    storage: InMemoryStorage,
    kb: KbSlug,
}

fn fixture(scenario: &str) -> Fixture {
    Fixture {
        storage: InMemoryStorage::default(),
        kb: kb_for(scenario),
    }
}

macro_rules! memory_scenario {
    ($name:ident) => {
        #[tokio::test]
        async fn $name() {
            let fixture = fixture(stringify!($name));
            scenarios::$name(&fixture.storage, &fixture.kb).await;
        }
    };
}

storage_integration_scenarios!(memory_scenario);
