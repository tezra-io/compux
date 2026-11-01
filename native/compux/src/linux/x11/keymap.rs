//! The X keyboard as compux reads it: which keycode, at which shift level, types
//! each character, taken from the server's own core mapping (`GetKeyboardMapping`).
//!
//! **Never remapped.** A character the layout cannot type is refused
//! (`text_outside_layout`, naming `paste`) rather than given a spare keycode:
//! remapping one changes the keymap of every client on the display and races the
//! settings daemon, which owns it.
//!
//! **Group 1 only.** XKB lays a key's symbols out in the core mapping as group 1
//! levels 1 and 2, group 2 levels 1 and 2, then group 1 levels 3 and 4. Text is
//! typed only while the keyboard is in its first group with no modifier held and
//! Caps Lock off ([`neutral`]), so level 2 is Shift and level 3 is
//! `ISO_Level3_Shift` (AltGr), and no other column is read.
//!
//! Pure: the mapping and the keyboard state are handed in.

use std::collections::HashMap;

use xkeysym::Keysym;

pub const SHIFT_L: u32 = 0xffe1;
pub const CONTROL_L: u32 = 0xffe3;
pub const ALT_L: u32 = 0xffe9;
pub const SUPER_L: u32 = 0xffeb;
pub const ISO_LEVEL3_SHIFT: u32 = 0xfe03;
pub const RETURN: u32 = 0xff0d;
pub const TAB: u32 = 0xff09;

/// The columns of the core mapping that hold group 1's levels 1 to 4.
const GROUP1_COLUMNS: [usize; 4] = [0, 1, 4, 5];

/// The core mask's Lock bit: Caps Lock (or Shift Lock) while it is locked.
pub const LOCK_MASK: u16 = 0x02;

/// How many characters a refusal names; the rest are counted, not listed.
const NAMED_OUTSIDE: usize = 8;

/// The core keyboard mapping: `per` keysyms for each keycode from `min`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    min: u8,
    per: u8,
    syms: Vec<u32>,
    /// Every character group 1 types, at its lowest level, then lowest keycode.
    chars: HashMap<char, (u8, u8)>,
}

/// One key press that types one character: the key, and the level keys held
/// around it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub keycode: u8,
    pub shift: Option<u8>,
    pub level3: Option<u8>,
}

/// Characters the layout cannot type, the first few by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outside {
    pub named: Vec<char>,
    pub count: usize,
}

impl Outside {
    /// The sentence a refusal carries.
    pub fn describe(&self) -> String {
        let named: Vec<String> = self.named.iter().map(|c| format!("{c:?}")).collect();
        let more = self.count.saturating_sub(self.named.len());
        let rest = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        format!(
            "the keyboard layout cannot type {}{rest}; compux never remaps a key to make room, \
             so nothing was typed: use `paste` for this text",
            named.join(", ")
        )
    }
}

/// The XKB keyboard state that decides whether text may be typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyboardState {
    /// 0 is the first group.
    pub group: u8,
    /// Modifiers held down or latched.
    pub held: u16,
    /// Modifiers locked; only Lock (Caps Lock) matters, NumLock does not.
    pub locked: u16,
}

impl Keymap {
    pub fn new(min: u8, per: u8, syms: Vec<u32>) -> Keymap {
        let chars = characters(min, per, &syms);
        Keymap {
            min,
            per,
            syms,
            chars,
        }
    }

    /// The keycode that types this keysym unshifted, for a modifier or a named
    /// key in a chord. `None` where the layout does not have it at level 1.
    pub fn keycode(&self, keysym: u32) -> Option<u8> {
        self.at_level(keysym, 0)
    }

    /// The strokes that type a key named by its keysym, at the lowest level it
    /// sits at, with the level keys it needs.
    pub fn stroke_for(&self, keysym: u32) -> Option<Stroke> {
        let (keycode, level) = (0..GROUP1_COLUMNS.len())
            .find_map(|level| self.at_level(keysym, level).map(|code| (code, level)))?;
        self.stroke(keycode, level)
    }

    /// The strokes that type `text`, one per character, or every character the
    /// layout cannot type. Nothing is typed unless every character can be.
    pub fn strokes(&self, text: &str) -> Result<Vec<Stroke>, Outside> {
        let mut strokes = Vec::new();
        let mut outside: Vec<char> = Vec::new();
        for c in text.chars() {
            match self.stroke_for_char(c) {
                Some(stroke) => strokes.push(stroke),
                None => outside.push(c),
            }
        }
        if outside.is_empty() {
            return Ok(strokes);
        }
        let count = outside.len();
        outside.dedup();
        let mut named: Vec<char> = Vec::new();
        for c in outside {
            if !named.contains(&c) && named.len() < NAMED_OUTSIDE {
                named.push(c);
            }
        }
        Err(Outside { named, count })
    }

    fn stroke_for_char(&self, c: char) -> Option<Stroke> {
        // A newline is the Return key, whose own character is a carriage return.
        let c = if c == '\n' { '\r' } else { c };
        let (keycode, level) = *self.chars.get(&c)?;
        self.stroke(keycode, usize::from(level))
    }

    /// A key at a level, with Shift for levels 2 and 4 and AltGr for 3 and 4.
    fn stroke(&self, keycode: u8, level: usize) -> Option<Stroke> {
        let shift = match level {
            1 | 3 => Some(self.keycode(SHIFT_L)?),
            _ => None,
        };
        let level3 = match level {
            2 | 3 => Some(self.keycode(ISO_LEVEL3_SHIFT)?),
            _ => None,
        };
        Some(Stroke {
            keycode,
            shift,
            level3,
        })
    }

    fn at_level(&self, keysym: u32, level: usize) -> Option<u8> {
        let column = GROUP1_COLUMNS[level];
        let per = usize::from(self.per);
        if keysym == 0 || column >= per {
            return None;
        }
        self.syms
            .chunks(per)
            .position(|row| row.get(column) == Some(&keysym))
            .and_then(|index| u8::try_from(index).ok())
            .and_then(|index| self.min.checked_add(index))
    }
}

/// Every character group 1 types, keyed to its lowest level, then its lowest
/// keycode. Keypad keys are left out: what they type depends on Num Lock.
fn characters(min: u8, per: u8, syms: &[u32]) -> HashMap<char, (u8, u8)> {
    let mut chars = HashMap::new();
    let per = usize::from(per);
    if per == 0 {
        return chars;
    }
    for (level, column) in GROUP1_COLUMNS.iter().enumerate() {
        for (index, row) in syms.chunks(per).enumerate() {
            let Some(&raw) = row.get(*column) else {
                continue;
            };
            let Some(c) = typed_by(raw) else {
                continue;
            };
            let (Ok(index), Ok(level)) = (u8::try_from(index), u8::try_from(level)) else {
                continue;
            };
            let Some(keycode) = min.checked_add(index) else {
                continue;
            };
            chars.entry(c).or_insert((keycode, level));
        }
    }
    chars
}

/// The character a keysym types, where it types one.
fn typed_by(raw: u32) -> Option<char> {
    let keysym = Keysym::new(raw);
    if raw == 0 || keysym.is_keypad_key() {
        return None;
    }
    keysym.key_char()
}

/// Whether text may be typed now: the first group, nothing held, Caps Lock off.
/// The sentence says what is not so.
pub fn neutral(state: KeyboardState) -> Result<(), String> {
    if state.group != 0 {
        return Err(format!(
            "the keyboard is in layout group {}, not the first, so the keys compux reads would \
             type other characters: switch back to the first layout",
            u16::from(state.group) + 1
        ));
    }
    if state.held != 0 {
        return Err(
            "a modifier key is held down, so every key compux types would carry it: let it go"
                .to_string(),
        );
    }
    if state.locked & LOCK_MASK != 0 {
        return Err(
            "Caps Lock is on, so letters would type in the other case: turn it off".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small US layout as the server's core mapping lays it out, 6 columns a key:
    // group 1 levels 1 and 2, group 2 levels 1 and 2, group 1 levels 3 and 4.
    const MIN: u8 = 8;
    const PER: u8 = 6;

    fn us() -> Keymap {
        let rows: Vec<[u32; 6]> = vec![
            [0, 0, 0, 0, 0, 0],                               // 8
            [0xff1b, 0, 0xff1b, 0, 0, 0],                     // 9 Escape
            [0x31, 0x21, 0x31, 0x21, 0xb9, 0xa1],             // 10 1 ! ¹ ¡
            [0x61, 0x41, 0x61, 0x41, 0xe6, 0xc6],             // 11 a A æ Æ
            [0x65, 0x45, 0x65, 0x45, 0x20ac, 0],              // 12 e E €(legacy) -
            [SHIFT_L, 0, SHIFT_L, 0, 0, 0],                   // 13
            [RETURN, 0, RETURN, 0, 0, 0],                     // 14
            [0x20, 0, 0x20, 0, 0, 0],                         // 15 space
            [ISO_LEVEL3_SHIFT, 0, ISO_LEVEL3_SHIFT, 0, 0, 0], // 16
            [0xffb1, 0xffb1, 0xffb1, 0xffb1, 0, 0],           // 17 KP_1
            [0x6c1, 0x6e1, 0x6c1, 0x6e1, 0, 0],               // 18 Cyrillic a, A
            [CONTROL_L, 0, CONTROL_L, 0, 0, 0],               // 19
            [0x7a, 0x5a, 0x7a, 0x5a, 0, 0],                   // 20 z Z
            [0x61, 0x41, 0x61, 0x41, 0, 0],                   // 21 a again, later
        ];
        Keymap::new(MIN, PER, rows.concat())
    }

    #[test]
    fn a_letter_is_its_key_unshifted_and_its_capital_is_the_same_key_with_shift() {
        let map = us();
        assert_eq!(
            map.strokes("aA").expect("both typable"),
            vec![
                Stroke {
                    keycode: 11,
                    shift: None,
                    level3: None
                },
                Stroke {
                    keycode: 11,
                    shift: Some(13),
                    level3: None
                },
            ]
        );
    }

    // The same character on two keys is typed by the lower keycode, and a
    // character at level 1 anywhere wins over the same at a higher level.
    #[test]
    fn the_lowest_level_then_the_lowest_keycode_types_a_character() {
        let map = us();
        assert_eq!(map.strokes("a").unwrap()[0].keycode, 11);
        assert_eq!(map.keycode(0x61), Some(11));
    }

    #[test]
    fn altgr_types_the_third_and_fourth_levels() {
        let map = us();
        assert_eq!(
            map.strokes("æÆ").expect("typable with AltGr"),
            vec![
                Stroke {
                    keycode: 11,
                    shift: None,
                    level3: Some(16)
                },
                Stroke {
                    keycode: 11,
                    shift: Some(13),
                    level3: Some(16)
                },
            ]
        );
    }

    #[test]
    fn a_newline_is_the_return_key_and_a_space_the_space_bar() {
        let map = us();
        assert_eq!(
            map.strokes("a\n ")
                .unwrap()
                .iter()
                .map(|s| s.keycode)
                .collect::<Vec<_>>(),
            vec![11, 14, 15]
        );
    }

    // A legacy keysym (Cyrillic) types its own character, not only Unicode's.
    #[test]
    fn a_legacy_keysym_types_its_character() {
        let map = us();
        assert_eq!(map.strokes("аА").unwrap()[1].keycode, 18);
    }

    // A keypad key types per Num Lock, so it never types a character here.
    #[test]
    fn the_keypad_never_types_text() {
        let mut rows = vec![0u32; 6 * 2];
        rows[0] = 0xffb1; // KP_1 at level 1
        let map = Keymap::new(MIN, PER, rows);
        assert!(map.strokes("1").is_err());
    }

    #[test]
    fn text_the_layout_cannot_type_is_refused_whole_and_names_what_is_missing() {
        let map = us();
        let outside = map
            .strokes("aéé😀b")
            .expect_err("é and the emoji are not here");
        assert_eq!(outside.named, vec!['é', '😀', 'b']);
        assert_eq!(outside.count, 4);
        let sentence = outside.describe();
        assert!(sentence.contains("paste"), "{sentence}");
        assert!(sentence.contains("'é'"), "{sentence}");
    }

    #[test]
    fn a_refusal_names_a_few_characters_and_counts_the_rest() {
        let map = us();
        let outside = map.strokes("bcdfghijklmnop").expect_err("none of these");
        assert_eq!(outside.named.len(), NAMED_OUTSIDE);
        assert_eq!(outside.count, 14);
        assert!(outside.describe().contains("and 6 more"));
    }

    #[test]
    fn a_named_key_is_found_by_its_keysym_and_a_missing_one_is_not() {
        let map = us();
        assert_eq!(map.keycode(CONTROL_L), Some(19));
        assert_eq!(map.keycode(SUPER_L), None);
        assert_eq!(
            map.stroke_for(0x5a),
            Some(Stroke {
                keycode: 20,
                shift: Some(13),
                level3: None
            })
        );
    }

    // No Shift key in the map: a capital cannot be typed, and says so.
    #[test]
    fn a_level_with_no_key_to_reach_it_is_outside_the_layout() {
        let rows: Vec<u32> = [[0x61u32, 0x41, 0, 0, 0, 0]].concat();
        let map = Keymap::new(MIN, PER, rows);
        assert!(map.strokes("a").is_ok());
        assert!(map.strokes("A").is_err());
    }

    #[test]
    fn text_is_typed_only_in_the_first_group_with_nothing_held_and_caps_lock_off() {
        assert_eq!(neutral(KeyboardState::default()), Ok(()));
        // Num Lock (Mod2) locked is fine.
        assert_eq!(
            neutral(KeyboardState {
                locked: 0x10,
                ..KeyboardState::default()
            }),
            Ok(())
        );
        let second = neutral(KeyboardState {
            group: 1,
            ..KeyboardState::default()
        });
        assert!(second.unwrap_err().contains("group 2"));
        let held = neutral(KeyboardState {
            held: 0x01,
            ..KeyboardState::default()
        });
        assert!(held.unwrap_err().contains("held"));
        let caps = neutral(KeyboardState {
            locked: LOCK_MASK,
            ..KeyboardState::default()
        });
        assert!(caps.unwrap_err().contains("Caps Lock"));
    }
}
