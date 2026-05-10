//! Text normalisation helpers for imported descriptions.
//!
//! Japanese banks routinely emit half-width katakana (`ｱｸﾒ`) in CSV exports
//! because the field width was budgeted for JIS X 0201 in the 80s. Downstream
//! tooling (GnuCash, search, classify) expects full-width katakana, so we
//! convert into [`Transaction::description_normalized`](crate::io::Transaction)
//! and leave `description_raw` untouched.

/// Convert half-width katakana (U+FF61–U+FF9F) to full-width katakana
/// (U+30A1–U+30FA), folding the spacing voiced (`ﾞ`) and semi-voiced (`ﾟ`)
/// marks into their voiced base char (e.g. `ｶﾞ` → `ガ`, `ﾊﾟ` → `パ`). Other
/// characters pass through unchanged.
pub fn halfwidth_kana_to_fullwidth(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let Some(base) = map_base(c) else {
            out.push(c);
            continue;
        };
        match chars.peek() {
            Some('\u{FF9E}') => {
                if let Some(voiced) = apply_voiced(base) {
                    chars.next();
                    out.push(voiced);
                    continue;
                }
            }
            Some('\u{FF9F}') => {
                if let Some(semi) = apply_semivoiced(base) {
                    chars.next();
                    out.push(semi);
                    continue;
                }
            }
            _ => {}
        }
        out.push(base);
    }
    out
}

fn map_base(c: char) -> Option<char> {
    Some(match c {
        '\u{FF61}' => '。',
        '\u{FF62}' => '「',
        '\u{FF63}' => '」',
        '\u{FF64}' => '、',
        '\u{FF65}' => '・',
        '\u{FF66}' => 'ヲ',
        '\u{FF67}' => 'ァ',
        '\u{FF68}' => 'ィ',
        '\u{FF69}' => 'ゥ',
        '\u{FF6A}' => 'ェ',
        '\u{FF6B}' => 'ォ',
        '\u{FF6C}' => 'ャ',
        '\u{FF6D}' => 'ュ',
        '\u{FF6E}' => 'ョ',
        '\u{FF6F}' => 'ッ',
        '\u{FF70}' => 'ー',
        '\u{FF71}' => 'ア',
        '\u{FF72}' => 'イ',
        '\u{FF73}' => 'ウ',
        '\u{FF74}' => 'エ',
        '\u{FF75}' => 'オ',
        '\u{FF76}' => 'カ',
        '\u{FF77}' => 'キ',
        '\u{FF78}' => 'ク',
        '\u{FF79}' => 'ケ',
        '\u{FF7A}' => 'コ',
        '\u{FF7B}' => 'サ',
        '\u{FF7C}' => 'シ',
        '\u{FF7D}' => 'ス',
        '\u{FF7E}' => 'セ',
        '\u{FF7F}' => 'ソ',
        '\u{FF80}' => 'タ',
        '\u{FF81}' => 'チ',
        '\u{FF82}' => 'ツ',
        '\u{FF83}' => 'テ',
        '\u{FF84}' => 'ト',
        '\u{FF85}' => 'ナ',
        '\u{FF86}' => 'ニ',
        '\u{FF87}' => 'ヌ',
        '\u{FF88}' => 'ネ',
        '\u{FF89}' => 'ノ',
        '\u{FF8A}' => 'ハ',
        '\u{FF8B}' => 'ヒ',
        '\u{FF8C}' => 'フ',
        '\u{FF8D}' => 'ヘ',
        '\u{FF8E}' => 'ホ',
        '\u{FF8F}' => 'マ',
        '\u{FF90}' => 'ミ',
        '\u{FF91}' => 'ム',
        '\u{FF92}' => 'メ',
        '\u{FF93}' => 'モ',
        '\u{FF94}' => 'ヤ',
        '\u{FF95}' => 'ユ',
        '\u{FF96}' => 'ヨ',
        '\u{FF97}' => 'ラ',
        '\u{FF98}' => 'リ',
        '\u{FF99}' => 'ル',
        '\u{FF9A}' => 'レ',
        '\u{FF9B}' => 'ロ',
        '\u{FF9C}' => 'ワ',
        '\u{FF9D}' => 'ン',
        // Orphan voicing marks — fold to spacing full-width forms.
        '\u{FF9E}' => '゛',
        '\u{FF9F}' => '゜',
        _ => return None,
    })
}

fn apply_voiced(c: char) -> Option<char> {
    Some(match c {
        'ウ' => 'ヴ',
        'カ' => 'ガ',
        'キ' => 'ギ',
        'ク' => 'グ',
        'ケ' => 'ゲ',
        'コ' => 'ゴ',
        'サ' => 'ザ',
        'シ' => 'ジ',
        'ス' => 'ズ',
        'セ' => 'ゼ',
        'ソ' => 'ゾ',
        'タ' => 'ダ',
        'チ' => 'ヂ',
        'ツ' => 'ヅ',
        'テ' => 'デ',
        'ト' => 'ド',
        'ハ' => 'バ',
        'ヒ' => 'ビ',
        'フ' => 'ブ',
        'ヘ' => 'ベ',
        'ホ' => 'ボ',
        _ => return None,
    })
}

fn apply_semivoiced(c: char) -> Option<char> {
    Some(match c {
        'ハ' => 'パ',
        'ヒ' => 'ピ',
        'フ' => 'プ',
        'ヘ' => 'ペ',
        'ホ' => 'ポ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_halfwidth_kana() {
        assert_eq!(halfwidth_kana_to_fullwidth("ｱｸﾒ"), "アクメ");
        assert_eq!(halfwidth_kana_to_fullwidth("ｲﾄｳｺｳｷﾞｮｳ"), "イトウコウギョウ");
    }

    #[test]
    fn voiced_and_semivoiced_marks_fold_into_base() {
        assert_eq!(halfwidth_kana_to_fullwidth("ｶﾞｷﾞｸﾞｹﾞｺﾞ"), "ガギグゲゴ");
        assert_eq!(halfwidth_kana_to_fullwidth("ﾊﾟﾋﾟﾌﾟﾍﾟﾎﾟ"), "パピプペポ");
        assert_eq!(halfwidth_kana_to_fullwidth("ﾊﾞﾋﾞﾌﾞﾍﾞﾎﾞ"), "バビブベボ");
        assert_eq!(halfwidth_kana_to_fullwidth("ｳﾞ"), "ヴ");
    }

    #[test]
    fn long_vowel_and_punctuation() {
        assert_eq!(halfwidth_kana_to_fullwidth("ｺｰﾋｰ"), "コーヒー");
        assert_eq!(
            halfwidth_kana_to_fullwidth("ｱ｡ｲ｢ｳ｣ｴ､ｵ･"),
            "ア。イ「ウ」エ、オ・"
        );
    }

    #[test]
    fn small_kana_and_sokuon() {
        assert_eq!(halfwidth_kana_to_fullwidth("ｷｬｯﾌﾟ"), "キャップ");
    }

    #[test]
    fn mixed_with_ascii_and_full_width() {
        assert_eq!(
            halfwidth_kana_to_fullwidth("ABC ｱｸﾒ 株式会社"),
            "ABC アクメ 株式会社"
        );
    }

    #[test]
    fn already_fullwidth_is_idempotent() {
        let s = "アクメ株式会社 ガビ";
        assert_eq!(halfwidth_kana_to_fullwidth(s), s);
    }

    #[test]
    fn orphan_voicing_marks_become_spacing_forms() {
        // ﾞ following an ASCII char (not voicable) should still be normalised
        // rather than left as half-width.
        assert_eq!(halfwidth_kana_to_fullwidth("Aﾞ"), "A゛");
        assert_eq!(halfwidth_kana_to_fullwidth("Aﾟ"), "A゜");
    }
}
