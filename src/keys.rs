pub const SHIFT: u8 = 0xE1;
pub const COMMAND: u8 = 0xE3;
pub const V: u8 = 0x19;

/// the hid key for a character on a us layout, and whether it needs shift
pub fn key(c: char) -> Option<(u8, bool)> {
    const SHIFTED_DIGITS: &str = "!@#$%^&*()";
    const PUNCTUATION: [(char, char, u8); 11] = [
        ('-', '_', 0x2D),
        ('=', '+', 0x2E),
        ('[', '{', 0x2F),
        (']', '}', 0x30),
        ('\\', '|', 0x31),
        (';', ':', 0x33),
        ('\'', '"', 0x34),
        ('`', '~', 0x35),
        (',', '<', 0x36),
        ('.', '>', 0x37),
        ('/', '?', 0x38),
    ];
    Some(match c {
        'a'..='z' => (0x04 + (c as u8 - b'a'), false),
        'A'..='Z' => (0x04 + (c as u8 - b'A'), true),
        '1'..='9' => (0x1E + (c as u8 - b'1'), false),
        '0' => (0x27, false),
        '\n' => (0x28, false),
        '\t' => (0x2B, false),
        ' ' => (0x2C, false),
        _ => {
            if let Some(i) = SHIFTED_DIGITS.find(c) {
                return Some((0x1E + i as u8, true));
            }
            let &(plain, _, usage) = PUNCTUATION.iter().find(|(p, s, _)| *p == c || *s == c)?;
            (usage, c != plain)
        }
    })
}

/// key presses that type out the text, none when a character can't be typed on a us keyboard
pub fn presses(text: &str) -> Option<Vec<(u8, bool)>> {
    let mut presses = Vec::new();
    for c in text.replace("\r\n", "\n").chars() {
        let (usage, shift) = key(c)?;
        if shift {
            presses.push((SHIFT, true));
        }
        presses.push((usage, true));
        presses.push((usage, false));
        if shift {
            presses.push((SHIFT, false));
        }
    }
    Some(presses)
}

#[test]
fn types_a_sentence() {
    let typed = presses("Hi!").unwrap();
    assert_eq!(
        typed,
        [
            (SHIFT, true),
            (0x0B, true),
            (0x0B, false),
            (SHIFT, false),
            (0x0C, true),
            (0x0C, false),
            (SHIFT, true),
            (0x1E, true),
            (0x1E, false),
            (SHIFT, false)
        ]
    );
    assert!(presses("café").is_none());
}
