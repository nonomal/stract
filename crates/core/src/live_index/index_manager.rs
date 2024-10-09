// Stract is an open source web search engine.
// Copyright (C) 2024 Stract ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>

use std::sync::Arc;

use chrono::Utc;

use super::{
    LiveIndex, AUTO_COMMIT_INTERVAL, COMPACT_INTERVAL, EVENT_LOOP_INTERVAL, PRUNE_INTERVAL,
};

pub struct IndexManager {
    index: Arc<LiveIndex>,
}

impl IndexManager {
    pub fn new(index: Arc<LiveIndex>) -> Self {
        Self { index }
    }

    pub fn run(self) {
        let mut last_commit = Utc::now();
        let mut last_prune = Utc::now();
        let mut last_compact = Utc::now();

        loop {
            if last_prune + PRUNE_INTERVAL < Utc::now() {
                crate::block_on(self.index.prune_segments());
                last_prune = Utc::now();
            }

            if last_commit + AUTO_COMMIT_INTERVAL < Utc::now()
                && crate::block_on(self.index.has_inserts())
            {
                crate::block_on(self.index.commit());
                last_commit = Utc::now();
            }

            if last_compact + COMPACT_INTERVAL < Utc::now() {
                if let Err(e) = crate::block_on(self.index.compact_segments_by_date()) {
                    tracing::error!("Failed to compact segments: {}", e);
                }

                last_compact = Utc::now();
            }

            std::thread::sleep(EVENT_LOOP_INTERVAL);
        }
    }
}
