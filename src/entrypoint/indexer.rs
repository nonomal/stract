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
use futures::prelude::*;
use indicatif::{ProgressBar, ProgressIterator, ProgressStyle};
use rusoto_core::Region;
use rusoto_s3::{GetObjectRequest, S3Client, S3};
use std::fs::File;
use std::io::{self, BufRead};
use tokio::io::AsyncReadExt;

use crate::warc::WarcFile;
use crate::webpage::Html;
use crate::{Config, Error, Result};

pub struct Indexer {
    warc_paths: Vec<String>,
    config: Config,
}

impl From<Config> for Indexer {
    fn from(config: Config) -> Self {
        let file = File::open(&config.warc_paths_file).unwrap();
        let mut warc_paths = Vec::new();

        for line in io::BufReader::new(file).lines() {
            warc_paths.push(line.unwrap());
        }

        Self { warc_paths, config }
    }
}

impl Indexer {
    pub async fn run(self) -> Result<()> {
        let pb = ProgressBar::new(self.warc_paths.len() as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner:.green} [{elapsed_precise}] [{wide_bar}] {pos:>7}/{len:7} ({eta})",
                )
                .progress_chars("#>-"),
        );

        stream::iter(self.warc_paths.into_iter().progress_with(pb))
            .map(|warc_path| {
                let source = self
                    .config
                    .warc_source
                    .as_ref()
                    .expect("Indexing needs a warc source")
                    .clone();

                tokio::spawn(async move { WarcFile::download(source, &warc_path).await })
            })
            .buffer_unordered(20)
            .map(|warc| {
                tokio::task::spawn_blocking(move || {
                    if warc.is_err() {
                        return;
                    }
                    let warc = warc.unwrap();

                    if warc.is_err() {
                        return;
                    }
                    let warc = warc.unwrap();

                    for record in warc.records().flatten() {
                        let _webpage = Html::parse(&record.response.body, &record.request.url);
                        // println!("TEST: {:?}", webpage.title());
                        // println!();
                    }
                    // panic!();
                })
            })
            .buffer_unordered(20)
            .collect::<Vec<_>>()
            .await;

        Ok(())
    }

    async fn download_from_http(warc_path: String, base_url: String) -> Result<Vec<u8>> {
        let mut url = base_url;
        if !url.ends_with('/') {
            url += "/";
        }
        url += &warc_path;

        let client = reqwest::Client::new();
        let res = client.get(url).send().await?;

        Ok(Vec::from(&res.bytes().await?[..]))
    }

    async fn download_from_s3(
        key: String,
        region_name: String,
        region_endpoint: String,
        bucket: String,
    ) -> Result<Vec<u8>> {
        let region = Region::Custom {
            name: region_name,
            endpoint: region_endpoint,
        };

        let client = S3Client::new(region);

        let obj = client
            .get_object(GetObjectRequest {
                bucket,
                key,
                ..Default::default()
            })
            .await?;

        let mut res = Vec::new();
        obj.body
            .ok_or(Error::S3DownloadError)?
            .into_async_read()
            .read_to_end(&mut res)
            .await?;

        Ok(res)
    }
}