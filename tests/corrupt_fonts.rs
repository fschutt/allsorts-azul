//! Fonts with corrupt tables give an error. They do not panic.
//!
//! Each case is the test font with one table changed. They are the panics that
//! mutating font files found: a cmap record that points past its table, and a
//! numberOfHMetrics of 0 in a font that is subset.

mod common;

use std::borrow::Cow;
use std::collections::BTreeMap;

use allsorts::binary::read::ReadScope;
use allsorts::error::ParseError;
use allsorts::font_data::FontData;
use allsorts::subset::{subset, CmapTarget, SubsetProfile};
use allsorts::tables::FontTableProvider;
use allsorts::{tag, Font};

use crate::common::read_fixture;

/// The tables of a font. A test changes one of them before the font is read.
struct Tables(BTreeMap<u32, Vec<u8>>);

impl FontTableProvider for Tables {
    fn table_data(&self, tag: u32) -> Result<Option<Cow<'_, [u8]>>, ParseError> {
        Ok(self.0.get(&tag).map(|data| Cow::Borrowed(data.as_slice())))
    }

    fn has_table(&self, tag: u32) -> bool {
        self.0.contains_key(&tag)
    }

    fn table_tags(&self) -> Option<Vec<u32>> {
        Some(self.0.keys().copied().collect())
    }
}

fn test_font() -> Tables {
    let buffer = read_fixture("tests/fonts/opentype/test-font.ttf");
    let font_file = ReadScope::new(&buffer)
        .read::<FontData<'_>>()
        .expect("read the font file");
    let provider = font_file.table_provider(0).expect("the first font");
    let tables = provider
        .table_tags()
        .expect("table tags")
        .into_iter()
        .map(|tag| {
            let data = provider.read_table_data(tag).expect("read a table");
            (tag, data.into_owned())
        })
        .collect();
    Tables(tables)
}

#[test]
fn an_unchanged_font_is_read_and_subset() {
    let font = test_font();
    assert!(subset(&font, &[0, 1, 2], &SubsetProfile::Minimal, CmapTarget::Unrestricted).is_ok());
    let font = Font::new(font).expect("read the font");
    assert!(!font.cmap_subtable_data().is_empty());
}

#[test]
fn a_cmap_record_that_points_past_the_table_is_an_error() {
    let mut font = test_font();
    let cmap = font.0.get_mut(&tag::CMAP).expect("a cmap table");
    // Header: version, numTables. Then one 8 byte record per table: platform, encoding,
    // and a u32 offset to the subtable.
    let num_tables = usize::from(u16::from_be_bytes([cmap[2], cmap[3]]));
    for record in 0..num_tables {
        let offset = 4 + record * 8 + 4;
        cmap[offset..offset + 4].copy_from_slice(&0x0040_0014u32.to_be_bytes());
    }

    // The font used to be accepted, and `cmap_subtable_data` then sliced the table from
    // an offset far past its end.
    match Font::new(font) {
        Err(ParseError::BadOffset) => {}
        Err(other) => panic!("unexpected error: {other:?}"),
        Ok(font) => panic!(
            "the font was accepted, its cmap subtable has {} bytes",
            font.cmap_subtable_data().len()
        ),
    }
}

#[test]
fn a_font_without_horizontal_metrics_cannot_be_subset() {
    let mut font = test_font();
    let hhea = font.0.get_mut(&tag::HHEA).expect("an hhea table");
    // numberOfHMetrics is the last field. 0 leaves no record to repeat for the other glyphs,
    // and the subsetter subtracted 1 from it.
    let end = hhea.len();
    hhea[end - 2..].copy_from_slice(&0u16.to_be_bytes());

    let result = subset(&font, &[0, 1, 2], &SubsetProfile::Minimal, CmapTarget::Unrestricted);
    assert!(result.is_err());
}
