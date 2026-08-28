use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) fn width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

pub(crate) fn truncate(value: &str, max_width: usize) -> String {
    if width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }

    let available = max_width - 1;
    let mut used = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > available {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

pub(crate) fn pad_right(value: &str, target_width: usize) -> String {
    let mut output = truncate(value, target_width);
    output.push_str(&" ".repeat(target_width.saturating_sub(width(&output))));
    output
}

#[cfg(test)]
mod tests {
    use super::{pad_right, truncate, width};

    #[test]
    fn handles_ascii_and_wide_characters_consistently() {
        assert_eq!(width("扫描器"), 6);
        assert_eq!(truncate("哈希值批量计算器", 8), "哈希值…");
        assert_eq!(truncate("ripgrep", 12), "ripgrep");
        assert_eq!(truncate("wide", 0), "");
        assert_eq!(width(&pad_right("工具", 8)), 8);
    }
}
