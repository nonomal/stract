// Stract is an open source web search engine.
// Copyright (C) 2023 Stract ApS
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

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{prelude::BASE64_STANDARD as BASE64_ENGINE, Engine};

use lending_iter::LendingIterator;
use tantivy::{
    collector::TopDocs,
    query::{BooleanQuery, BoostQuery, MoreLikeThisQuery, Occur, QueryClone, TermQuery},
    schema::{BytesOptions, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value},
    tokenizer::Tokenizer,
    DocAddress, IndexReader, IndexWriter, Searcher, TantivyDocument, Term,
};

use crate::{
    image_store::{EntityImageStore, Image, ImageStore},
    inverted_index::merge_tantivy_segments,
    tokenizer::fields::DefaultTokenizer,
    Result,
};

use self::entity::{Entity, Link, Span};
pub(crate) mod entity;

fn schema() -> Schema {
    let mut builder = tantivy::schema::Schema::builder();

    builder.add_text_field(
        "title",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(DefaultTokenizer::as_str())
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    builder.add_text_field(
        "abstract",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(DefaultTokenizer::as_str())
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    builder.add_bytes_field("info", BytesOptions::default().set_stored());
    builder.add_bytes_field("links", BytesOptions::default().set_stored());
    builder.add_text_field(
        "has_image",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    builder.add_text_field(
        "image",
        TextOptions::default()
            .set_indexing_options(TextFieldIndexing::default())
            .set_stored(),
    );

    builder.build()
}

fn entity_to_tantivy(entity: Entity, schema: &tantivy::schema::Schema) -> TantivyDocument {
    let mut doc = TantivyDocument::new();

    doc.add_text(schema.get_field("title").unwrap(), entity.title);
    doc.add_text(
        schema.get_field("abstract").unwrap(),
        entity.page_abstract.text,
    );
    doc.add_bytes(
        schema.get_field("info").unwrap(),
        &bincode::encode_to_vec(&entity.info, common::bincode_config()).unwrap(),
    );
    doc.add_bytes(
        schema.get_field("links").unwrap(),
        &bincode::encode_to_vec(&entity.page_abstract.links, common::bincode_config()).unwrap(),
    );
    let has_image = if entity.image.is_some() {
        "true"
    } else {
        "false"
    };

    doc.add_text(schema.get_field("has_image").unwrap(), has_image);
    doc.add_text(
        schema.get_field("image").unwrap(),
        entity.image.unwrap_or_default(),
    );

    doc
}

#[derive(Debug, serde::Serialize, serde::Deserialize, bincode::Encode, bincode::Decode, Clone)]
pub struct StoredEntity {
    pub title: String,
    pub entity_abstract: String,
    pub image_id: Option<String>,
    pub related_entities: Vec<EntityMatch>,
    pub best_info: Vec<(String, Span)>,
    pub links: Vec<Link>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, bincode::Encode, bincode::Decode, Clone)]
pub struct EntityMatch {
    pub entity: StoredEntity,
    pub score: f32,
}

pub struct EntityIndex {
    image_store: EntityImageStore,
    writer: Option<IndexWriter>,
    reader: IndexReader,
    tv_index: tantivy::Index,
    schema: Arc<Schema>,
    stopwords: HashSet<String>,
    path: PathBuf,
}

impl EntityIndex {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        if !path.as_ref().exists() {
            fs::create_dir_all(path.as_ref())?;
        }

        let schema = schema();
        let tv_path = path.as_ref().join("inverted_index");
        let tantivy_index = if tv_path.exists() {
            tantivy::Index::open_in_dir(&tv_path)?
        } else {
            fs::create_dir_all(&tv_path)?;
            tantivy::Index::create_in_dir(&tv_path, schema.clone())?
        };

        let stopwords: HashSet<String> = include_str!("../../stopwords/English.txt")
            .lines()
            .take(50)
            .map(str::to_ascii_lowercase)
            .collect();

        tantivy_index.tokenizers().register(
            DefaultTokenizer::as_str(),
            DefaultTokenizer::with_stopwords(stopwords.clone().into_iter().collect()),
        );

        let image_store = EntityImageStore::open(path.as_ref().join("images"));

        let reader = tantivy_index.reader()?;

        Ok(Self {
            image_store,
            writer: None,
            reader,
            tv_index: tantivy_index,
            schema: Arc::new(schema),
            stopwords,
            path: path.as_ref().to_path_buf(),
        })
    }

    pub fn prepare_writer(&mut self) {
        self.writer = Some(self.tv_index.writer(10_000_000_000).unwrap());
        self.image_store.prepare_writer();
    }

    fn best_info(&self, info: Vec<(String, Span)>) -> Vec<(String, Span)> {
        info.into_iter().take(5).collect()
    }

    pub fn insert(&mut self, entity: Entity) {
        let doc = entity_to_tantivy(entity, &self.schema);
        self.writer
            .as_mut()
            .expect("writer not prepared")
            .add_document(doc)
            .unwrap();
    }

    pub fn commit(&mut self) {
        self.writer
            .as_mut()
            .expect("writer not prepared")
            .commit()
            .unwrap();
        self.image_store.flush();
        self.reader.reload().unwrap();
    }

    fn related_entities(&self, doc: DocAddress, image_id: Option<&String>) -> Vec<EntityMatch> {
        let searcher = self.reader.searcher();
        let more_like_this_query = MoreLikeThisQuery::builder()
            .with_min_doc_frequency(1)
            .with_min_term_frequency(1)
            .with_min_word_length(2)
            .with_boost_factor(1.0)
            .with_document(doc);

        let image_query = TermQuery::new(
            Term::from_field_text(self.schema.get_field("has_image").unwrap(), "true"),
            IndexRecordOption::WithFreqsAndPositions,
        );

        let query = BooleanQuery::from(vec![
            (Occur::Must, more_like_this_query.box_clone()),
            (Occur::Must, image_query.box_clone()),
        ]);

        let mut images = HashSet::new();

        if let Some(image_id) = image_id {
            images.insert(image_id.clone());
        }

        match searcher.search(&query, &TopDocs::with_limit(100)) {
            Ok(result) => result
                .into_iter()
                .filter(|(_, related_doc)| doc != *related_doc)
                .map(|(score, doc_address)| {
                    let entity =
                        self.retrieve_stored_entity(&searcher, doc_address, false, false, false);

                    EntityMatch { entity, score }
                })
                .filter(|entity_match| {
                    if let Some(image_id) = &entity_match.entity.image_id {
                        let res = !images.contains(image_id);

                        images.insert(image_id.clone());

                        res
                    } else {
                        false
                    }
                })
                .take(4)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    pub fn search(&self, query: &str) -> Option<EntityMatch> {
        let searcher = self.reader.searcher();

        let title = self.schema.get_field("title").unwrap();
        let entity_abstract = self.schema.get_field("abstract").unwrap();

        let mut term_queries = Vec::new();
        let mut tokenizer = DefaultTokenizer::default();
        let mut stream = tokenizer.token_stream(query);
        let mut it = tantivy::tokenizer::TokenStream::iter(&mut stream);
        while let Some(token) = it.next() {
            if self.stopwords.contains(&token.text) {
                continue;
            }

            term_queries.push((
                Occur::Must,
                BoostQuery::new(
                    TermQuery::new(
                        Term::from_field_text(title, &token.text),
                        IndexRecordOption::WithFreqsAndPositions,
                    )
                    .box_clone(),
                    5.0,
                )
                .box_clone(),
            ));

            term_queries.push((
                Occur::Should,
                TermQuery::new(
                    Term::from_field_text(entity_abstract, &token.text),
                    IndexRecordOption::WithFreqsAndPositions,
                )
                .box_clone(),
            ));
        }

        let query = BooleanQuery::from(term_queries);

        searcher
            .search(&query, &TopDocs::with_limit(1))
            .unwrap()
            .first()
            .map(|(score, doc_address)| {
                let entity = self.retrieve_stored_entity(&searcher, *doc_address, true, true, true);

                EntityMatch {
                    entity,
                    score: *score,
                }
            })
    }

    fn retrieve_stored_entity(
        &self,
        searcher: &Searcher,
        doc_address: DocAddress,
        get_related: bool,
        decode_info: bool,
        get_links: bool,
    ) -> StoredEntity {
        let title = self.schema.get_field("title").unwrap();
        let entity_abstract = self.schema.get_field("abstract").unwrap();
        let info = self.schema.get_field("info").unwrap();
        let links = self.schema.get_field("links").unwrap();
        let image_field = self.schema.get_field("image").unwrap();

        let doc: TantivyDocument = searcher.doc(doc_address).unwrap();
        let title = doc
            .get_first(title)
            .and_then(|val| val.as_str().map(|s| s.to_string()))
            .unwrap();

        let entity_abstract = doc
            .get_first(entity_abstract)
            .and_then(|val| val.as_str().map(|s| s.to_string()))
            .unwrap();

        let info = if decode_info {
            let (info, _) = bincode::decode_from_slice(
                doc.get_first(info).and_then(|val| val.as_bytes()).unwrap(),
                common::bincode_config(),
            )
            .unwrap();

            info
        } else {
            Vec::new()
        };

        let best_info = self.best_info(info);

        let image_id = doc
            .get_first(image_field)
            .and_then(|val| val.as_str().map(|s| s.to_string()))
            .unwrap();

        let image_id = if !image_id.is_empty() {
            BASE64_ENGINE.encode(image_id)
        } else {
            String::new()
        };

        let image_id = if !image_id.is_empty() && self.retrieve_image(&image_id).is_some() {
            Some(image_id)
        } else {
            None
        };

        let related_entities = if get_related {
            self.related_entities(doc_address, image_id.as_ref())
        } else {
            Vec::new()
        };

        let links: Vec<Link> = if get_links {
            let (links, _) = bincode::decode_from_slice(
                doc.get_first(links).and_then(|val| val.as_bytes()).unwrap(),
                common::bincode_config(),
            )
            .unwrap();

            links
        } else {
            Vec::new()
        };

        StoredEntity {
            title,
            entity_abstract,
            image_id,
            related_entities,
            best_info,
            links,
        }
    }

    pub fn retrieve_image(&self, key: &str) -> Option<Image> {
        let key = BASE64_ENGINE.decode(key).ok()?;
        let key = String::from_utf8(key).ok()?;

        self.image_store.get(&key)
    }

    pub fn insert_image(&mut self, name: String, image: Image) {
        self.image_store.insert(name, image);
    }

    pub fn merge_all_segments(&mut self) -> Result<()> {
        self.image_store.merge_all_segments();
        let base_path = Path::new(&self.path);
        let segments: Vec<_> = self.tv_index.load_metas()?.segments.into_iter().collect();

        merge_tantivy_segments(
            self.writer.as_mut().expect("writer has not been prepared"),
            segments,
            base_path,
            1,
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopwords_title_ignored() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut index = EntityIndex::open(&temp_dir).unwrap();
        index.prepare_writer();

        index.insert(Entity {
            article_url: String::new(),
            is_disambiguation: false,
            title: "the ashes".to_string(),
            page_abstract: Span {
                text: String::new(),
                links: Vec::new(),
            },
            info: Vec::new(),
            image: None,
        });

        index.commit();

        assert!(index.search("the").is_none());
        assert_eq!(
            index.search("ashes").unwrap().entity.title.as_str(),
            "the ashes"
        );
        assert_eq!(
            index.search("the ashes").unwrap().entity.title.as_str(),
            "the ashes"
        );
    }

    #[test]
    fn image() {
        let temp_dir = crate::gen_temp_dir().unwrap();
        let mut index = EntityIndex::open(&temp_dir).unwrap();
        index.prepare_writer();

        index.insert(Entity {
            article_url: String::new(),
            is_disambiguation: false,
            title: "the ashes".to_string(),
            page_abstract: Span {
                text: String::new(),
                links: Vec::new(),
            },
            info: Vec::new(),
            image: Some("test".to_string()),
        });

        index.commit();

        let image = Image::empty(32, 32);
        index.insert_image("test".to_string(), image.clone());

        index.commit();

        assert_eq!(
            index.search("ashes").unwrap().entity.image_id,
            Some(BASE64_ENGINE.encode("test"))
        );

        assert!(index
            .retrieve_image(&index.search("ashes").unwrap().entity.image_id.unwrap())
            .is_some());
    }
}
