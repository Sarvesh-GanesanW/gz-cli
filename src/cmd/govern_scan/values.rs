//! Whether one value has the shape of a kind of personal data, and how to
//! show it masked. Pure functions: no I/O, no column context beyond whether
//! the column's name already suggests the kind.

use std::net::{IpAddr, Ipv4Addr};

/// Words that end a street address line.
const STREET_WORDS: [&str; 24] = [
    "street",
    "st",
    "road",
    "rd",
    "avenue",
    "ave",
    "lane",
    "ln",
    "boulevard",
    "blvd",
    "drive",
    "dr",
    "nagar",
    "marg",
    "sector",
    "block",
    "floor",
    "apt",
    "flat",
    "house",
    "colony",
    "layout",
    "cross",
    "main",
];
/// The fourth letter of a PAN: who holds it (person, company, trust, …).
const PAN_HOLDER_TYPES: &str = "ABCFGHLJPT";
/// IBAN length per country, for the countries whose banks issue them most.
const IBAN_LENGTHS: [(&str, usize); 36] = [
    ("AD", 24),
    ("AE", 23),
    ("AT", 20),
    ("BE", 16),
    ("BG", 22),
    ("BH", 22),
    ("CH", 21),
    ("CY", 28),
    ("CZ", 24),
    ("DE", 22),
    ("DK", 18),
    ("EE", 20),
    ("ES", 24),
    ("FI", 18),
    ("FR", 27),
    ("GB", 22),
    ("GR", 27),
    ("HR", 21),
    ("HU", 28),
    ("IE", 22),
    ("IT", 27),
    ("KW", 30),
    ("LT", 20),
    ("LU", 20),
    ("LV", 21),
    ("MT", 31),
    ("NL", 18),
    ("NO", 15),
    ("PL", 28),
    ("PT", 25),
    ("QA", 29),
    ("RO", 24),
    ("SA", 24),
    ("SE", 24),
    ("SI", 19),
    ("SK", 24),
];
/// The oldest a living person's date of birth plausibly is.
const MAX_AGE_YEARS: i64 = 120;
const VERHOEFF_MULTIPLY: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];
const VERHOEFF_PERMUTE: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];

/// The digits of `value` when it holds only digits and the separators
/// people write numbers with (space, dash, dot, parentheses).
fn digits_of(value: &str) -> Option<String> {
    let allowed = |c: char| c.is_ascii_digit() || matches!(c, ' ' | '-' | '.' | '(' | ')');
    if !value.chars().all(allowed) {
        return None;
    }
    Some(value.chars().filter(char::is_ascii_digit).collect())
}

fn all_same(digits: &str) -> bool {
    digits.chars().all(|c| digits.starts_with(c))
}

pub fn is_email(value: &str) -> bool {
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
    let labels: Vec<&str> = domain.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    let tld_ok = labels
        .last()
        .is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    local_ok && labels.len() >= 2 && labels.iter().all(label_ok) && tld_ok
}

/// A phone number in international form (`+CC …`), or a national Indian,
/// North American or UK number. Without a name hint the number must be
/// written with a `+` or separators, so plain IDs are not taken for phones.
pub fn is_phone(value: &str, hinted: bool) -> bool {
    let (international, rest) = match value.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let Some(digits) = digits_of(rest) else {
        return false;
    };
    let formatted = international || digits.len() != rest.len();
    if (!hinted && !formatted) || all_same(&digits) || value.parse::<Ipv4Addr>().is_ok() {
        return false;
    }
    if international {
        return international_phone(&digits);
    }
    national_phone(&digits)
}

fn international_phone(digits: &str) -> bool {
    if let Some(national) = digits.strip_prefix("91") {
        return indian_mobile(national);
    }
    if let Some(national) = digits.strip_prefix('1') {
        if digits.len() == 11 {
            return north_american(national);
        }
    }
    if let Some(national) = digits.strip_prefix("44") {
        return national.len() == 10;
    }
    (8..=15).contains(&digits.len())
}

fn national_phone(digits: &str) -> bool {
    let without_trunk = digits.strip_prefix('0').unwrap_or(digits);
    indian_mobile(without_trunk)
        || north_american(digits)
        || digits.strip_prefix('1').is_some_and(north_american)
        || (digits.len() == 11 && digits.starts_with("07"))
}

fn indian_mobile(digits: &str) -> bool {
    digits.len() == 10 && digits.starts_with(['6', '7', '8', '9'])
}

/// NANP: area code and exchange both start 2-9.
fn north_american(digits: &str) -> bool {
    let bytes = digits.as_bytes();
    digits.len() == 10 && bytes[0] >= b'2' && bytes[3] >= b'2'
}

pub fn is_payment_card(value: &str) -> bool {
    let Some(digits) = digits_of(value) else {
        return false;
    };
    (13..=19).contains(&digits.len()) && !all_same(&digits) && luhn_valid(&digits)
}

pub fn luhn_valid(digits: &str) -> bool {
    let sum: u32 = digits
        .bytes()
        .rev()
        .enumerate()
        .map(|(position, byte)| {
            let digit = u32::from(byte - b'0');
            if position % 2 == 1 {
                let doubled = digit * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                digit
            }
        })
        .sum();
    sum % 10 == 0
}

pub fn is_ip_address(value: &str) -> bool {
    value.trim().parse::<IpAddr>().is_ok()
}

/// An Indian Permanent Account Number: `AAAPA1234A`, the fourth letter
/// saying what kind of holder it is.
pub fn is_pan(value: &str) -> bool {
    let pan = value.trim().to_ascii_uppercase();
    let bytes = pan.as_bytes();
    pan.len() == 10
        && bytes[..5].iter().all(u8::is_ascii_uppercase)
        && PAN_HOLDER_TYPES.as_bytes().contains(&bytes[3])
        && bytes[5..9].iter().all(u8::is_ascii_digit)
        && bytes[9].is_ascii_uppercase()
}

/// A 12-digit Aadhaar number: it never starts with 0 or 1, and its last
/// digit is a Verhoeff check digit.
pub fn is_aadhaar(value: &str) -> bool {
    let Some(digits) = digits_of(value) else {
        return false;
    };
    digits.len() == 12
        && !digits.starts_with(['0', '1'])
        && !all_same(&digits)
        && verhoeff_valid(&digits)
}

pub fn verhoeff_valid(digits: &str) -> bool {
    let check = digits
        .bytes()
        .rev()
        .enumerate()
        .fold(0u8, |check, (i, byte)| {
            let digit = VERHOEFF_PERMUTE[i % 8][usize::from(byte - b'0')];
            VERHOEFF_MULTIPLY[usize::from(check)][usize::from(digit)]
        });
    check == 0
}

/// A US Social Security number, `123-45-6789` (or nine digits when the
/// column's name says SSN), excluding numbers never issued.
pub fn is_ssn(value: &str, hinted: bool) -> bool {
    let value = value.trim();
    let bytes = value.as_bytes();
    let written = value.len() == 11
        && [3, 6].iter().all(|&i| matches!(bytes[i], b'-' | b' '))
        && bytes[3] == bytes[6];
    let plain = hinted && value.len() == 9;
    if !written && !plain {
        return false;
    }
    let Some(digits) = digits_of(value) else {
        return false;
    };
    if digits.len() != 9 {
        return false;
    }
    let (area, group, serial) = (&digits[..3], &digits[3..5], &digits[5..]);
    area != "000" && area != "666" && !area.starts_with('9') && group != "00" && serial != "0000"
}

pub fn is_iban(value: &str) -> bool {
    let iban: String = value
        .chars()
        .filter(|c| *c != ' ')
        .collect::<String>()
        .to_ascii_uppercase();
    if !iban.chars().all(|c| c.is_ascii_alphanumeric()) || iban.len() < 4 {
        return false;
    }
    let country = &iban[..2];
    let expected = IBAN_LENGTHS
        .iter()
        .find(|(code, _)| *code == country)
        .map(|(_, length)| *length);
    expected == Some(iban.len())
        && iban[2..4].chars().all(|c| c.is_ascii_digit())
        && iban_mod97(&iban) == 1
}

/// ISO 13616: move the first four characters to the end, letters become
/// 10–35, and the number mod 97 must be 1.
fn iban_mod97(iban: &str) -> u32 {
    let rearranged = iban[4..].chars().chain(iban[..4].chars());
    rearranged.fold(0u32, |remainder, c| {
        let value = c.to_digit(36).unwrap_or(0);
        let shift = if value >= 10 { 100 } else { 10 };
        (remainder * shift + value) % 97
    })
}

/// A date a living person could have been born on: ISO (`1990-04-17`,
/// also the start of a timestamp), or day/month/year with `/`, `-` or `.`.
pub fn is_birth_date(value: &str, current_year: i64) -> bool {
    let date = value.trim();
    let date = date.get(..10).unwrap_or(date);
    let parts: Vec<&str> = date.split(['-', '/', '.']).collect();
    let [a, b, c] = parts[..] else {
        return false;
    };
    let number = |text: &str| text.parse::<i64>().ok();
    let (year, month, day) = match (a.len(), c.len()) {
        (4, _) => (number(a), number(b), number(c)),
        (_, 4) => {
            // Day first unless that cannot be a month-first date.
            let (first, second) = (number(a), number(b));
            if first.is_some_and(|first| first > 12) {
                (number(c), second, first)
            } else {
                (number(c), first, second)
            }
        }
        _ => return false,
    };
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    let plausible_year = (current_year - MAX_AGE_YEARS..=current_year).contains(&year);
    plausible_year && (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// One to four capitalised words of letters, as people's names are written.
pub fn has_name_shape(value: &str) -> bool {
    let words: Vec<&str> = value.split_whitespace().collect();
    let word_ok = |word: &&str| {
        let mut chars = word.chars();
        chars.next().is_some_and(char::is_uppercase)
            && word.chars().all(|c| c.is_alphabetic() || "'-.".contains(c))
    };
    (1..=4).contains(&words.len())
        && value.trim().chars().count() >= 2
        && value.chars().count() <= 60
        && words.iter().all(word_ok)
}

/// A house number with a street word, or a comma-separated line.
pub fn has_address_shape(value: &str) -> bool {
    let lower = value.to_lowercase();
    let has_digit = lower.chars().any(|c| c.is_ascii_digit());
    let has_letters = lower.chars().filter(|c| c.is_alphabetic()).count() >= 3;
    let has_street_word = lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| STREET_WORDS.contains(&word));
    lower.len() >= 8 && has_digit && has_letters && (has_street_word || lower.contains(','))
}

/// `value` with every letter and digit but the last `keep` replaced by `*`;
/// separators stay so the shape shows.
pub fn keep_last(value: &str, keep: usize) -> String {
    let total = value.chars().filter(|c| c.is_alphanumeric()).count();
    let mut seen = 0;
    value
        .chars()
        .map(|c| {
            if !c.is_alphanumeric() {
                return c;
            }
            seen += 1;
            if seen + keep > total {
                c
            } else {
                '*'
            }
        })
        .collect()
}

/// Every word cut to its first letter (digits fully hidden).
pub fn initials(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| match word.chars().next() {
            Some(first) if first.is_alphabetic() => format!("{first}***"),
            _ => "***".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn masked_email(value: &str) -> String {
    let (local, domain) = value.split_once('@').unwrap_or((value, ""));
    let (host, tld) = domain.rsplit_once('.').unwrap_or((domain, ""));
    let first = |text: &str| text.chars().next().map(String::from).unwrap_or_default();
    format!("{}***@{}***.{tld}", first(local), first(host))
}

pub fn masked_ip(value: &str) -> String {
    let value = value.trim();
    match value.split_once(['.', ':']) {
        Some((first, _)) if value.contains('.') => format!("{first}.*.*.*"),
        Some((first, _)) => format!("{first}:*"),
        None => "*".to_string(),
    }
}

pub fn masked_digits(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_digit() { '*' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_need_a_local_part_and_a_dotted_domain() {
        assert!(is_email("asha.k+news@mail.example.in"));
        assert!(!is_email("asha@localhost"));
        assert!(!is_email("@example.com"));
        assert!(!is_email("asha at example.com"));
        assert!(!is_email("a@b.c0m"));
    }

    #[test]
    fn phones_in_country_formats() {
        assert!(is_phone("+91 98765 43210", false));
        assert!(is_phone("(415) 555-2671", false));
        assert!(is_phone("+44 20 7946 0958", false));
        assert!(is_phone("+49 30 901820", false));
        assert!(is_phone("098765-43210", false));
        assert!(
            !is_phone("+91 12345 67890", false),
            "Indian mobiles start 6-9"
        );
        assert!(
            !is_phone("(115) 555-2671", false),
            "NANP area codes start 2-9"
        );
    }

    #[test]
    fn plain_numbers_are_phones_only_when_the_column_says_so() {
        assert!(!is_phone("9876543210", false));
        assert!(is_phone("9876543210", true));
        assert!(
            !is_phone("192.168.10.1", true),
            "an IP address is not a phone"
        );
        assert!(!is_phone("9999999999", true));
    }

    #[test]
    fn cards_must_pass_luhn() {
        assert!(is_payment_card("4111 1111 1111 1111"));
        assert!(is_payment_card("5500-0000-0000-0004"));
        assert!(is_payment_card("378282246310005"));
        assert!(!is_payment_card("4111 1111 1111 1112"));
        assert!(!is_payment_card("0000000000000000"));
        assert!(!is_payment_card("4111-1111-1111-111x"));
    }

    #[test]
    fn aadhaar_needs_a_verhoeff_check_digit() {
        assert!(verhoeff_valid("2363"));
        assert!(!verhoeff_valid("2364"));
        assert!(is_aadhaar("2345 6789 0124"));
        assert!(!is_aadhaar("2345 6789 0123"));
        assert!(!is_aadhaar("1345 6789 0124"), "Aadhaar never starts with 1");
    }

    #[test]
    fn pan_has_a_holder_type_letter() {
        assert!(is_pan("ABCPE1234F"));
        assert!(is_pan("abcpe1234f"));
        assert!(!is_pan("ABCDE1234F"), "D is not a holder type");
        assert!(!is_pan("ABCPE12345"));
    }

    #[test]
    fn ssn_excludes_numbers_never_issued() {
        assert!(is_ssn("123-45-6789", false));
        assert!(!is_ssn("123456789", false));
        assert!(is_ssn("123456789", true));
        for never in [
            "000-12-3456",
            "666-12-3456",
            "912-12-3456",
            "123-00-4567",
            "123-45-0000",
        ] {
            assert!(!is_ssn(never, false), "{never}");
        }
        assert!(!is_ssn("123-45 6789", false));
    }

    #[test]
    fn iban_needs_its_country_length_and_mod97() {
        assert!(is_iban("GB82 WEST 1234 5698 7654 32"));
        assert!(is_iban("DE89370400440532013000"));
        assert!(!is_iban("GB82 WEST 1234 5698 7654 33"));
        assert!(!is_iban("DE8937040044053201300"), "one digit short for DE");
        assert!(!is_iban("ZZ89370400440532013000"));
    }

    #[test]
    fn ip_addresses_v4_and_v6() {
        assert!(is_ip_address("10.0.12.7"));
        assert!(is_ip_address("2001:db8::1"));
        assert!(!is_ip_address("10.0.12"));
        assert!(!is_ip_address("300.1.1.1"));
    }

    #[test]
    fn birth_dates_must_be_plausible() {
        assert!(is_birth_date("1990-04-17", 2026));
        assert!(is_birth_date("1990-04-17 00:00:00", 2026));
        assert!(is_birth_date("17/04/1990", 2026));
        assert!(is_birth_date("04/17/1990", 2026));
        assert!(!is_birth_date("1890-04-17", 2026));
        assert!(!is_birth_date("2030-01-01", 2026));
        assert!(!is_birth_date("1990-13-01", 2026));
        assert!(!is_birth_date("yesterday", 2026));
    }

    #[test]
    fn names_and_addresses_by_shape() {
        assert!(has_name_shape("Asha Kumar"));
        assert!(has_name_shape("Mary-Jane O'Neil"));
        assert!(!has_name_shape("asha kumar"));
        assert!(!has_name_shape("Widget 3000"));
        assert!(has_address_shape("12 MG Road, Bengaluru"));
        assert!(has_address_shape("221B Baker Street"));
        assert!(!has_address_shape("Bengaluru"));
    }

    #[test]
    fn masks_hide_all_but_a_little() {
        assert_eq!(keep_last("4111 1111 1111 1111", 4), "**** **** **** 1111");
        assert_eq!(keep_last("+91 98765 43210", 2), "+** ***** ***10");
        assert_eq!(masked_email("asha.k@example.in"), "a***@e***.in");
        assert_eq!(masked_ip("10.0.12.7"), "10.*.*.*");
        assert_eq!(masked_ip("2001:db8::1"), "2001:*");
        assert_eq!(masked_digits("1990-04-17"), "****-**-**");
        assert_eq!(initials("Asha Kumar"), "A*** K***");
        assert_eq!(initials("12 MG Road"), "*** M*** R***");
    }
}
