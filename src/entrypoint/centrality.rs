// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
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
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use std::path::Path;

use crate::{ranking::centrality_store::CentralityStore, webgraph::WebgraphBuilder};

pub struct CentralityEntrypoint {}

impl CentralityEntrypoint {
    pub fn run<P: AsRef<Path>>(webgraph_path: P, output_path: P) {
        let graph = WebgraphBuilder::new(webgraph_path).with_host_graph().open();

        let mut centrality_store = CentralityStore::new(output_path);

        centrality_store.append(
            graph
                .host_harmonic_centrality()
                .into_iter()
                .map(|(node, centrality)| (node.name, centrality)),
        );
    }
}
