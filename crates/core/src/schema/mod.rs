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
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

pub mod fast_field;
pub mod text_field;

use tantivy::schema::{BytesOptions, DateOptions, NumericOptions, TextOptions};

pub use fast_field::{DataType, FastFieldEnum};
pub use text_field::TextFieldEnum;

use self::{fast_field::FastField, text_field::TextField};

pub const FLOAT_SCALING: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Fast(FastFieldEnum),
    Text(TextFieldEnum),
}

impl Field {
    #[inline]
    pub fn get(field_id: usize) -> Option<Field> {
        if field_id < TextFieldEnum::num_variants() {
            return Some(Field::Text(TextFieldEnum::get(field_id).unwrap()));
        }
        let field_id = field_id - TextFieldEnum::num_variants();

        if field_id < FastFieldEnum::num_variants() {
            return Some(Field::Fast(FastFieldEnum::get(field_id).unwrap()));
        }
        let _field_id = field_id - FastFieldEnum::num_variants();

        None
    }

    #[inline]
    pub fn all() -> impl Iterator<Item = Field> {
        TextFieldEnum::all()
            .map(Field::Text)
            .chain(FastFieldEnum::all().map(Field::Fast))
    }

    pub fn has_pos(&self) -> bool {
        match self {
            Field::Fast(_) => false,
            Field::Text(text) => text.has_pos(),
        }
    }

    pub fn indexing_option(&self) -> IndexingOption {
        match self {
            Field::Text(f) => f.indexing_option(),
            Field::Fast(f) => f.indexing_option(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Field::Text(f) => f.name(),
            Field::Fast(f) => f.name(),
        }
    }

    pub fn is_searchable(&self) -> bool {
        match self {
            Field::Text(f) => f.is_searchable(),
            Field::Fast(_) => false,
        }
    }

    pub fn as_text(&self) -> Option<TextFieldEnum> {
        match self {
            Field::Fast(_) => None,
            Field::Text(field) => Some(*field),
        }
    }

    pub fn as_fast(&self) -> Option<FastFieldEnum> {
        match self {
            Field::Fast(field) => Some(*field),
            Field::Text(_) => None,
        }
    }
}

pub fn create_schema() -> tantivy::schema::Schema {
    let mut builder = tantivy::schema::Schema::builder();

    for field in Field::all() {
        match field.indexing_option() {
            IndexingOption::Text(options) => builder.add_text_field(field.name(), options),
            IndexingOption::Integer(options) => builder.add_u64_field(field.name(), options),
            IndexingOption::DateTime(options) => builder.add_date_field(field.name(), options),
            IndexingOption::Bytes(options) => builder.add_bytes_field(field.name(), options),
        };
    }

    builder.build()
}

pub enum IndexingOption {
    Text(TextOptions),
    Integer(NumericOptions),
    DateTime(DateOptions),
    Bytes(BytesOptions),
}
