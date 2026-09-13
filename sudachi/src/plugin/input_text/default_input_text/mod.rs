/*
 * Copyright (c) 2021-2024 Works Applications Co., Ltd.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::OnceLock;

use aho_corasick::{
    AhoCorasick, AhoCorasickBuilder, AhoCorasickKind, Anchored, MatchKind, StartKind,
};
use serde::Deserialize;
use serde_json::Value;
use unicode_normalization::char::canonical_combining_class;
use unicode_normalization::{is_nfkc_quick, IsNormalized, UnicodeNormalization};

use crate::config::{Config, ConfigError};
use crate::dic::grammar::Grammar;
use crate::hash::RoMu;
use crate::input_text::{InputBuffer, InputEditor};
use crate::plugin::input_text::InputTextPlugin;
use crate::prelude::*;

#[cfg(test)]
mod tests;

const DEFAULT_REWRITE_DEF_FILE: &str = "rewrite.def";
const DEFAULT_REWRITE_DEF_BYTES: &[u8] = include_bytes!("../../../../../resources/rewrite.def");

/// Provides basic normalization of the input text
#[derive(Default)]
pub struct DefaultInputTextPlugin {
    /// Set of characters to skip normalization
    ignore_normalize_set: HashSet<char, RoMu>,
    /// Mapping from a character to the maximum char_length of possible replacement
    key_lengths: HashMap<char, usize>,
    /// Replacement mapping
    replace_char_map: HashMap<String, String>,
    /// Checks whether the string contains symbols to normalize
    checker: Option<AhoCorasick>,
    replacements: Vec<String>,
}

/// Struct corresponds with raw config json file.
#[allow(non_snake_case)]
#[derive(Deserialize)]
struct PluginSettings {
    rewriteDef: Option<PathBuf>,
}

/// Bitmap over the Basic Multilingual Plane of characters which are
/// "trivially normalized": NFKC quick check is Yes, the canonical combining
/// class is 0 and there is no lowercase mapping.
///
/// A string which consists only of such characters is guaranteed to be
/// [`IsNormalized::Yes`] for [`is_nfkc_quick`] and does not need lowercasing,
/// so the plugin can take the fast path without consulting the (comparatively
/// slow, binary-search based) Unicode tables for every character.
/// Almost all characters of Japanese text belong to this set.
struct TrivialChars {
    bits: Box<[u64]>,
}

impl TrivialChars {
    const BMP_SIZE: usize = 0x10000;

    fn compute() -> TrivialChars {
        let mut bits = vec![0u64; Self::BMP_SIZE / 64].into_boxed_slice();
        for cp in 0..Self::BMP_SIZE as u32 {
            let ch = match char::from_u32(cp) {
                Some(ch) => ch,
                None => continue,
            };
            let trivial = canonical_combining_class(ch) == 0
                && !ch.is_uppercase()
                && is_nfkc_quick(std::iter::once(ch)) == IsNormalized::Yes;
            if trivial {
                bits[(cp / 64) as usize] |= 1u64 << (cp % 64);
            }
        }
        TrivialChars { bits }
    }

    fn get() -> &'static TrivialChars {
        static INSTANCE: OnceLock<TrivialChars> = OnceLock::new();
        INSTANCE.get_or_init(TrivialChars::compute)
    }

    /// Returns true if it is known that the character needs no normalization.
    /// False means that the slow check is needed, not that the character
    /// is not normalized.
    #[inline]
    fn is_trivial(&self, ch: char) -> bool {
        let cp = ch as usize;
        if cp >= Self::BMP_SIZE {
            return false;
        }
        (self.bits[cp / 64] >> (cp % 64)) & 1 == 1
    }
}

impl DefaultInputTextPlugin {
    /// Loads rewrite definition
    ///
    /// Definition syntax:
    ///     Ignored normalize:
    ///         Each line contains a character
    ///     Replace char list:
    ///         Each line contains two strings separated by white spaces
    ///         Plugin replaces the first by the second
    ///         Same target string cannot be defined multiple times
    ///     Empty or line starts with "#" will be ignored
    fn read_rewrite_lists<T: BufRead>(&mut self, reader: T) -> SudachiResult<()> {
        let mut ignore_normalize_set = HashSet::with_hasher(RoMu::new());
        let mut key_lengths = HashMap::new();
        let mut replace_char_map = HashMap::new();
        for (i, line) in reader.lines().enumerate() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<_> = line.split_whitespace().collect();

            // ignored normalize list
            if cols.len() == 1 {
                if cols[0].chars().count() != 1 {
                    return Err(SudachiError::InvalidDataFormat(
                        i,
                        format!("{} is not character", cols[0]),
                    ));
                }
                ignore_normalize_set.insert(cols[0].chars().next().unwrap());
                continue;
            }
            // replace char list
            if cols.len() == 2 {
                if replace_char_map.contains_key(cols[0]) {
                    return Err(SudachiError::InvalidDataFormat(
                        i,
                        format!("{} is already defined", cols[0]),
                    ));
                }
                let first_char = cols[0].chars().next().unwrap();
                let n_char = cols[0].chars().count();
                if key_lengths.get(&first_char).copied().unwrap_or(0) < n_char {
                    key_lengths.insert(first_char, n_char);
                }
                replace_char_map.insert(cols[0].to_string(), cols[1].to_string());
                continue;
            }
            return Err(SudachiError::InvalidDataFormat(i, "".to_string()));
        }

        self.ignore_normalize_set = ignore_normalize_set;
        self.key_lengths = key_lengths;
        self.replace_char_map = replace_char_map;

        let mut values: Vec<String> = Vec::new();
        let mut keys: Vec<String> = Vec::new();

        for (k, v) in self.replace_char_map.iter() {
            keys.push(k.clone());
            values.push(v.clone());
        }

        self.checker = Some(
            AhoCorasickBuilder::new()
                .kind(Some(AhoCorasickKind::DFA))
                .match_kind(MatchKind::LeftmostLongest)
                .start_kind(StartKind::Both)
                .build(keys.clone())
                .map_err(|e| {
                    ConfigError::InvalidFormat(format!("failed to parse rewrite.def: {e:?}"))
                })?,
        );

        self.replacements = values;

        Ok(())
    }

    #[inline]
    fn should_ignore(&self, ch: char) -> bool {
        self.ignore_normalize_set.contains(&ch)
    }

    /// Fast case: lowercasing is not needed and the string is already in NFKC
    /// Use AhoCorasick automaton to find all replacements and replace them
    ///
    /// Ignores are not used here, forced replacements have higher priority
    /// Fast version does not need to walk every character!
    fn replace_fast<'a>(
        &'a self,
        buffer: &InputBuffer,
        mut replacer: InputEditor<'a>,
    ) -> SudachiResult<InputEditor<'a>> {
        let cur = buffer.current();
        let checker = self.checker.as_ref().unwrap();

        let ac_input = aho_corasick::Input::new(cur).anchored(Anchored::No);

        for m in checker.find_iter(ac_input) {
            let replacement = self.replacements[m.pattern()].as_str();
            replacer.replace_ref(m.start()..m.end(), replacement);
        }

        Ok(replacer)
    }

    /// Slow case: need to handle lowercasing or NFKC normalization
    ///
    /// Replacements from the rewrite definition have higher priority
    /// and are found with the automaton, exactly as in the fast case.
    /// Characters between the replacements are normalized one by one.
    fn replace_slow<'a>(
        &'a self,
        buffer: &InputBuffer,
        mut replacer: InputEditor<'a>,
    ) -> SudachiResult<InputEditor<'a>> {
        let cur = buffer.current();
        let checker = self.checker.as_ref().unwrap();

        let ac_input = aho_corasick::Input::new(cur).anchored(Anchored::No);

        let mut min_offset = 0;
        for m in checker.find_iter(ac_input) {
            self.normalize_slow(cur, min_offset..m.start(), &mut replacer);
            let replacement = self.replacements[m.pattern()].as_str();
            replacer.replace_ref(m.range(), replacement);
            min_offset = m.end();
        }
        self.normalize_slow(cur, min_offset..cur.len(), &mut replacer);

        Ok(replacer)
    }

    /// Normalize (lowercase + NFKC) characters of the byte range of `cur`
    fn normalize_slow<'a>(
        &'a self,
        cur: &str,
        range: std::ops::Range<usize>,
        replacer: &mut InputEditor<'a>,
    ) {
        let trivial = TrivialChars::get();
        let base = range.start;
        for (rel_offset, ch) in cur[range].char_indices() {
            if trivial.is_trivial(ch) {
                continue;
            }
            let offset = base + rel_offset;

            let need_lowercase = ch.is_uppercase();
            let need_nkfc =
                !self.should_ignore(ch) && is_nfkc_quick(std::iter::once(ch)) != IsNormalized::Yes;

            // iterator types are incompatible, so calls can't be moved outside branches
            match (need_lowercase, need_nkfc) {
                //no need to do anything
                (false, false) => continue,
                // only lowercasing
                (true, false) => {
                    let chars = ch.to_lowercase();
                    self.handle_normalization_slow(chars, replacer, offset, ch.len_utf8(), ch)
                }
                // only normalization
                (false, true) => {
                    let chars = std::iter::once(ch).nfkc();
                    self.handle_normalization_slow(chars, replacer, offset, ch.len_utf8(), ch)
                }
                // both
                (true, true) => {
                    let chars = ch.to_lowercase().nfkc();
                    self.handle_normalization_slow(chars, replacer, offset, ch.len_utf8(), ch)
                }
            }
        }
    }

    fn handle_normalization_slow<'a, I: Iterator<Item = char>>(
        &'a self,
        mut data: I,
        replacer: &mut InputEditor<'a>,
        start: usize,
        len: usize,
        ch: char,
    ) {
        if let Some(ch2) = data.next() {
            if ch2 != ch {
                replacer.replace_char_iter(start..start + len, ch2, data)
            }
        }
    }
}

impl InputTextPlugin for DefaultInputTextPlugin {
    fn set_up(
        &mut self,
        settings: &Value,
        config: &Config,
        _grammar: &Grammar,
    ) -> SudachiResult<()> {
        let settings: PluginSettings = serde_json::from_value(settings.clone())?;

        let rewrite_file_path = config.complete_path(
            settings
                .rewriteDef
                .unwrap_or_else(|| DEFAULT_REWRITE_DEF_FILE.into()),
        );

        if rewrite_file_path.is_ok() {
            let reader = BufReader::new(fs::File::open(rewrite_file_path?)?);
            self.read_rewrite_lists(reader)?;
        } else {
            let reader = BufReader::new(DEFAULT_REWRITE_DEF_BYTES);
            self.read_rewrite_lists(reader)?;
        }

        Ok(())
    }

    fn rewrite_impl<'a>(
        &'a self,
        buffer: &InputBuffer,
        edit: InputEditor<'a>,
    ) -> SudachiResult<InputEditor<'a>> {
        let cur = buffer.current();

        // fast check first, falling back to the full Unicode tables only
        // when there is a character which is not known to be trivial
        let trivial = TrivialChars::get();
        if cur.chars().all(|c| trivial.is_trivial(c)) {
            return self.replace_fast(buffer, edit);
        }

        let need_nkfc = is_nfkc_quick(cur.chars()) != IsNormalized::Yes;

        let need_lowercase = cur.chars().any(|c| c.is_uppercase());

        if need_nkfc || need_lowercase {
            self.replace_slow(buffer, edit)
        } else {
            self.replace_fast(buffer, edit)
        }
    }
}
