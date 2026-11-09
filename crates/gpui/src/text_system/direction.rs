use unicode_bidi::BidiClass;

/// The base direction used to lay out a paragraph of text.
///
/// This determines the paragraph embedding level for the Unicode
/// bidirectional algorithm (UAX #9): an RTL base places the first logical run
/// at the right edge of the paragraph and resolves neutral characters
/// (punctuation, whitespace, digits) against an RTL context, while an LTR base
/// keeps the paragraph left-anchored as usual.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextDirection {
    /// Left-to-right paragraph direction (the default).
    #[default]
    LeftToRight,
    /// Right-to-left paragraph direction.
    RightToLeft,
}

impl TextDirection {
    /// Whether this direction is right-to-left.
    pub fn is_rtl(self) -> bool {
        matches!(self, Self::RightToLeft)
    }

    /// Whether this direction is left-to-right.
    pub fn is_ltr(self) -> bool {
        matches!(self, Self::LeftToRight)
    }

    /// The UAX #9 paragraph embedding level for this direction.
    pub fn bidi_level(self) -> unicode_bidi::Level {
        if self.is_rtl() {
            unicode_bidi::Level::rtl()
        } else {
            unicode_bidi::Level::ltr()
        }
    }
}

/// Detects the base direction of a paragraph from its content.
///
/// The direction is decided by the majority of strong directional characters
/// (bidi classes `L` versus `R`/`AL`), falling back to the first strong
/// character when the votes are tied. Relying on the first strong character
/// alone would misclassify paragraphs that begin with an English product
/// name or technical term followed by Persian/Arabic prose, which is why the
/// full paragraph is counted.
///
/// Paragraphs without any strong directional content (numbers, punctuation,
/// whitespace, or text in neutral-only scripts) are treated as left-to-right.
pub fn detect_text_direction(text: &str) -> TextDirection {
    let mut ltr_count = 0usize;
    let mut rtl_count = 0usize;
    let mut first_strong = None::<TextDirection>;

    for char in text.chars() {
        match unicode_bidi::bidi_class(char) {
            BidiClass::L => {
                ltr_count += 1;
                first_strong.get_or_insert(TextDirection::LeftToRight);
            }
            BidiClass::R | BidiClass::AL => {
                rtl_count += 1;
                first_strong.get_or_insert(TextDirection::RightToLeft);
            }
            _ => {}
        }
    }

    match rtl_count.cmp(&ltr_count) {
        std::cmp::Ordering::Greater => TextDirection::RightToLeft,
        std::cmp::Ordering::Less => TextDirection::LeftToRight,
        // Ties (including paragraphs with no strong characters at all)
        // resolve to the first strong character when there is one.
        std::cmp::Ordering::Equal => first_strong.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_persian_paragraph() {
        assert_eq!(
            detect_text_direction("سلام دنیا بر روی زمین"),
            TextDirection::RightToLeft
        );
    }

    #[test]
    fn test_detect_persian_paragraph_starting_with_english_term() {
        assert_eq!(
            detect_text_direction("Zed بهترین ویرایشگر کد برای فارسی است"),
            TextDirection::RightToLeft
        );
    }

    #[test]
    fn test_detect_english_paragraph() {
        assert_eq!(
            detect_text_direction("The quick brown fox jumps over the lazy dog"),
            TextDirection::LeftToRight
        );
    }

    #[test]
    fn test_detect_english_paragraph_with_one_persian_word() {
        assert_eq!(
            detect_text_direction("This editor supports سلام text"),
            TextDirection::LeftToRight
        );
    }

    #[test]
    fn test_detect_mixed_persian_and_english() {
        assert_eq!(
            detect_text_direction("این متن شامل کلمه editor و زبان فارسی است"),
            TextDirection::RightToLeft
        );
    }

    #[test]
    fn test_detect_tie_falls_back_to_first_strong_character() {
        // A single Persian word and a single English word: votes are tied, so
        // the first strong character decides.
        assert_eq!(
            detect_text_direction("سلام world"),
            TextDirection::RightToLeft
        );
        assert_eq!(
            detect_text_direction("سلام world"),
            detect_text_direction("سلام world")
        );
        assert_eq!(
            detect_text_direction("hello دنیا"),
            TextDirection::LeftToRight
        );
    }

    #[test]
    fn test_detect_neutral_only_text_is_left_to_right() {
        assert_eq!(
            detect_text_direction("12345 !!!"),
            TextDirection::LeftToRight
        );
        assert_eq!(detect_text_direction(""), TextDirection::LeftToRight);
    }

    #[test]
    fn test_detect_persian_with_numbers_urls_and_punctuation() {
        assert_eq!(
            detect_text_direction("در سال ۱۴۰۴ نسخه 1.2 منتشر شد: https://zed.dev (مهم)"),
            TextDirection::RightToLeft
        );
        assert_eq!(
            detect_text_direction("از تابع `main()` و فایل config.json استفاده کن."),
            TextDirection::RightToLeft
        );
    }

    #[test]
    fn test_detect_arabic_paragraph() {
        assert_eq!(
            detect_text_direction("مرحبا بكم في محرر الأكواد زد"),
            TextDirection::RightToLeft
        );
    }
}
