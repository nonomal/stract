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

use super::{tokenize, MergePointer, Result};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, BinaryHeap},
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

use fst::{Automaton, IntoStreamer, Streamer};

const DISCOUNT: f64 = 0.4;

#[derive(
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Clone,
    serde::Serialize,
    serde::Deserialize,
    bincode::Encode,
    bincode::Decode,
)]
pub struct Ngram {
    terms: Vec<String>,
}

pub struct StoredNgram {
    combined: String,
}

impl From<Ngram> for StoredNgram {
    fn from(ngram: Ngram) -> Self {
        Self {
            combined: ngram.terms.join(" "),
        }
    }
}

impl AsRef<[u8]> for StoredNgram {
    fn as_ref(&self) -> &[u8] {
        self.combined.as_bytes()
    }
}

pub struct StupidBackoffTrainer {
    max_ngram_size: usize,
    ngrams: BTreeMap<Ngram, u64>,
    rotated_ngrams: BTreeMap<Ngram, u64>,
    n_counts: Vec<u64>,
}

impl StupidBackoffTrainer {
    pub fn new(max_ngram_size: usize) -> Self {
        Self {
            max_ngram_size,
            ngrams: BTreeMap::new(),
            rotated_ngrams: BTreeMap::new(),
            n_counts: vec![0; max_ngram_size],
        }
    }

    pub fn train(&mut self, tokens: &[String]) {
        for window in tokens.windows(self.max_ngram_size) {
            for i in 1..=window.len() {
                let ngram = Ngram {
                    terms: window[..i].to_vec(),
                };

                self.ngrams
                    .entry(ngram)
                    .and_modify(|e| *e += 1)
                    .or_insert(1);

                self.n_counts[i - 1] += 1;
            }

            let mut ngram = Ngram {
                terms: window.to_vec(),
            };
            ngram.terms.rotate_left(1);
            self.rotated_ngrams
                .entry(ngram)
                .and_modify(|e| *e += 1)
                .or_insert(1);
        }
    }

    pub fn build<P: AsRef<Path>>(self, path: P) -> Result<()> {
        if !path.as_ref().exists() {
            std::fs::create_dir_all(path.as_ref())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path.as_ref().join("ngrams.bin"))?;

        let wtr = BufWriter::new(file);

        let mut builder = fst::MapBuilder::new(wtr)?;

        for (ngram, freq) in self.ngrams {
            builder.insert(StoredNgram::from(ngram), freq)?;
        }

        builder.finish()?;

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path.as_ref().join("rotated_ngrams.bin"))?;

        let wtr = BufWriter::new(file);

        let mut builder = fst::MapBuilder::new(wtr)?;

        for (ngram, freq) in self.rotated_ngrams {
            builder.insert(StoredNgram::from(ngram), freq)?;
        }

        builder.finish()?;

        let f = File::create(path.as_ref().join("n_counts.bin"))?;
        let mut wrt = BufWriter::new(f);

        bincode::encode_into_std_write(&self.n_counts, &mut wrt, common::bincode_config())?;
        wrt.flush()?;

        Ok(())
    }
}

fn merge_streams(
    mut builder: fst::MapBuilder<BufWriter<File>>,
    streams: Vec<fst::map::Stream<'_, fst::automaton::AlwaysMatch>>,
) -> Result<()> {
    let mut pointers: Vec<_> = streams
        .into_iter()
        .map(|stream| MergePointer {
            term: String::new(),
            value: 0,
            stream,
            is_finished: false,
        })
        .collect();

    for pointer in pointers.iter_mut() {
        pointer.advance();
    }

    let mut pointers: BinaryHeap<_> = pointers.into_iter().map(Reverse).collect();

    loop {
        let (term, mut freq, is_finished) = {
            match pointers.peek_mut() {
                Some(mut pointer) => {
                    let res = (
                        pointer.0.term.clone(),
                        pointer.0.value,
                        pointer.0.is_finished,
                    );
                    pointer.0.advance();
                    res
                }
                None => break,
            }
        };

        if is_finished {
            break;
        }

        while let Some(mut other) = pointers.peek_mut() {
            if other.0.term != term || other.0.is_finished {
                break;
            }

            freq += other.0.value;
            other.0.advance();
        }

        builder.insert(term, freq)?;
    }

    builder.finish()?;

    Ok(())
}

pub struct StupidBackoff {
    ngrams: fst::Map<memmap2::Mmap>,
    rotated_ngrams: fst::Map<memmap2::Mmap>,
    n_counts: Vec<u64>,
    folder: PathBuf,
}

impl StupidBackoff {
    pub fn open<P: AsRef<Path>>(folder: P) -> Result<Self> {
        let mmap = unsafe { memmap2::Mmap::map(&File::open(folder.as_ref().join("ngrams.bin"))?)? };
        let ngrams = fst::Map::new(mmap)?;

        let mmap = unsafe {
            memmap2::Mmap::map(&File::open(folder.as_ref().join("rotated_ngrams.bin"))?)?
        };
        let rotated_ngrams = fst::Map::new(mmap)?;

        let file = File::open(folder.as_ref().join("n_counts.bin"))?;
        let mut reader = std::io::BufReader::new(file);
        let n_counts = bincode::decode_from_std_read(&mut reader, common::bincode_config())?;

        Ok(Self {
            ngrams,
            rotated_ngrams,
            n_counts,
            folder: folder.as_ref().to_path_buf(),
        })
    }

    pub fn merge<P: AsRef<Path>>(models: Vec<Self>, folder: P) -> Result<Self> {
        if !folder.as_ref().exists() {
            std::fs::create_dir_all(folder.as_ref())?;
        }
        let n_counts = models
            .iter()
            .fold(vec![0; models[0].n_counts.len()], |mut acc, m| {
                for (i, n) in m.n_counts.iter().enumerate() {
                    acc[i] += n;
                }

                acc
            });

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(folder.as_ref().join("n_counts.bin"))?;

        let mut wrt = BufWriter::new(file);
        bincode::encode_into_std_write(&n_counts, &mut wrt, common::bincode_config())?;
        wrt.flush()?;

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(folder.as_ref().join("ngrams.bin"))?;

        let wtr = BufWriter::new(file);
        let builder = fst::MapBuilder::new(wtr)?;

        let streams: Vec<_> = models.iter().map(|d| d.ngrams.stream()).collect();

        merge_streams(builder, streams)?;

        let mmap = unsafe { memmap2::Mmap::map(&File::open(folder.as_ref().join("ngrams.bin"))?)? };
        let ngrams = fst::Map::new(mmap)?;

        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(folder.as_ref().join("rotated_ngrams.bin"))?;

        let wtr = BufWriter::new(file);
        let builder = fst::MapBuilder::new(wtr)?;

        let streams: Vec<_> = models.iter().map(|d| d.rotated_ngrams.stream()).collect();

        merge_streams(builder, streams)?;

        let mmap = unsafe {
            memmap2::Mmap::map(&File::open(folder.as_ref().join("rotated_ngrams.bin"))?)?
        };
        let rotated_ngrams = fst::Map::new(mmap)?;

        for model in models {
            std::fs::remove_dir_all(model.folder)?;
        }

        Ok(Self {
            ngrams,
            rotated_ngrams,
            n_counts,
            folder: folder.as_ref().to_path_buf(),
        })
    }

    pub fn freq(&self, words: &[String]) -> Option<u64> {
        if words.len() >= self.ngrams.len() || words.is_empty() {
            return None;
        }

        let ngram = StoredNgram {
            combined: words.join(" "),
        };

        self.ngrams.get(ngram)
    }

    pub fn log_prob<S: NextWordsStrategy>(&self, words: &[String], strat: S) -> f64 {
        if words.len() >= self.ngrams.len() || words.is_empty() {
            return -(self.n_counts[0] as f64).log2();
        }

        let mut strat = strat;
        if let Some(freq) = self.freq(words) {
            if let Some(next_freq) = self.freq(strat.inverse().next_words(words)) {
                (freq as f64).log2() - (next_freq as f64).log2()
            } else {
                (freq as f64).log2() - (self.n_counts[words.len() - 1] as f64).log2()
            }
        } else {
            DISCOUNT.log2() + self.log_prob(strat.next_words(words), strat)
        }
    }

    pub fn prob<S: NextWordsStrategy>(&self, words: &[String], strat: S) -> f64 {
        self.log_prob(words, strat).exp2()
    }

    pub fn contexts(&self, word: &str) -> Vec<(Vec<String>, u64)> {
        let q = word.to_string() + " ";
        let automaton = fst::automaton::Str::new(&q).starts_with();

        let mut stream = self.rotated_ngrams.search(automaton).into_stream();

        let mut contexts = Vec::new();

        while let Some((ngram, freq)) = stream.next() {
            if let Ok(ngram) = std::str::from_utf8(ngram) {
                let mut ngram = tokenize(ngram);
                ngram.rotate_right(1);
                contexts.push((ngram, freq));
            }
        }

        contexts
    }
}

pub trait NextWordsStrategy: Sized {
    type Inv: NextWordsStrategy;

    fn next_words<'a>(&mut self, words: &'a [String]) -> &'a [String];
    fn inverse(self) -> Self::Inv;
}

pub struct LeftToRight;

impl NextWordsStrategy for LeftToRight {
    type Inv = RightToLeft;

    fn next_words<'a>(&mut self, words: &'a [String]) -> &'a [String] {
        &words[1..]
    }

    fn inverse(self) -> Self::Inv {
        RightToLeft
    }
}

pub struct RightToLeft;

impl NextWordsStrategy for RightToLeft {
    type Inv = LeftToRight;

    fn next_words<'a>(&mut self, words: &'a [String]) -> &'a [String] {
        &words[..words.len() - 1]
    }

    fn inverse(self) -> Self::Inv {
        LeftToRight
    }
}

#[derive(Default)]
pub struct IntoMiddle {
    last_left: bool,
}

impl NextWordsStrategy for IntoMiddle {
    type Inv = IntoMiddle;

    fn next_words<'a>(&mut self, words: &'a [String]) -> &'a [String] {
        let res = if self.last_left {
            &words[1..]
        } else {
            &words[..words.len() - 1]
        };

        self.last_left = !self.last_left;

        res
    }

    fn inverse(self) -> Self::Inv {
        // TODO: this is not entirely correct.
        // to score b in [a, b, c] we should score [a, c] in the rotated ngrams.
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gen_temp_dir;

    #[test]
    fn test_contexts() {
        let mut trainer = StupidBackoffTrainer::new(3);

        trainer.train(&tokenize(
            "a b c d e f g h i j k l m n o p q r s t u v w x y z",
        ));

        let temp_dir = gen_temp_dir().unwrap();

        trainer.build(&temp_dir).unwrap();

        let model = StupidBackoff::open(&temp_dir).unwrap();

        assert_eq!(
            model.contexts("b"),
            vec![(vec!["a".to_string(), "b".to_string(), "c".to_string()], 1)]
        );

        assert_eq!(model.n_counts, vec![24, 24, 24]);
    }

    #[test]
    fn test_merge() {
        let mut a = StupidBackoffTrainer::new(3);

        a.train(&tokenize(
            "a b c d e f g h i j k l m n o p q r s t u v w x y z",
        ));

        let temp_dir = gen_temp_dir().unwrap();

        a.build(&temp_dir.as_ref().join("a")).unwrap();

        let a = StupidBackoff::open(&temp_dir.as_ref().join("a")).unwrap();

        let mut b = StupidBackoffTrainer::new(3);

        b.train(&tokenize(
            "a b c d e f g h i j k l m n o p q r s t u v w x y z",
        ));

        b.build(&temp_dir.as_ref().join("b")).unwrap();

        let b = StupidBackoff::open(&temp_dir.as_ref().join("b")).unwrap();

        let model = StupidBackoff::merge(vec![a, b], &temp_dir.as_ref().join("merged")).unwrap();

        assert_eq!(model.n_counts, vec![48, 48, 48]);

        let model = StupidBackoff::open(&temp_dir.as_ref().join("merged")).unwrap();

        assert_eq!(model.n_counts, vec![48, 48, 48]);
    }
}
