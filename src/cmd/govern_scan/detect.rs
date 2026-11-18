//! Which kind of personal data a column holds, judged from its name and a
//! sample of its values, with how sure the judgement is.
//!
//! A column is flagged when enough of its non-empty values have a kind's
//! shape: half of them, or a fifth when the column's name already names the
//! kind. Checksums (Luhn, Verhoeff, IBAN mod 97) and strict formats make a
//! high rate convincing; phone numbers, dates of birth, names and addresses
//! are looser shapes, so they need the name to agree (dates, names and
//! addresses are not flagged on values alone) and count for less.

use serde::Serialize;

use super::values;

/// Share of values that flags a column on their shape alone.
const FLAG_RATE: f64 = 0.5;
/// Share that flags a column whose name already names the kind.
const HINTED_FLAG_RATE: f64 = 0.2;
/// Share at which a strict format or checksum is convincing.
const CONVINCING_RATE: f64 = 0.9;
/// Share at which a loose shape (name, address, date) counts as a match.
const LOOSE_SHAPE_RATE: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Email,
    Phone,
    PaymentCard,
    IpAddress,
    Pan,
    Aadhaar,
    Ssn,
    Iban,
    DateOfBirth,
    Name,
    Address,
}

/// Most specific first: when two kinds match a column equally, the one
/// with the stricter test wins.
const KINDS: [Kind; 11] = [
    Kind::PaymentCard,
    Kind::Aadhaar,
    Kind::Iban,
    Kind::Pan,
    Kind::Ssn,
    Kind::Email,
    Kind::IpAddress,
    Kind::Phone,
    Kind::DateOfBirth,
    Kind::Name,
    Kind::Address,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strength {
    /// A checksum or a format nothing else has.
    Strict,
    /// A format other numbers can share.
    Shaped,
    /// Only believable with the column's name: dates, names, addresses.
    NameLed,
}

impl Kind {
    /// The tag value written for the kind (`pii=<tag>`).
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Email => "email",
            Kind::Phone => "phone",
            Kind::PaymentCard => "payment_card",
            Kind::IpAddress => "ip_address",
            Kind::Pan => "pan",
            Kind::Aadhaar => "aadhaar",
            Kind::Ssn => "ssn",
            Kind::Iban => "iban",
            Kind::DateOfBirth => "date_of_birth",
            Kind::Name => "name",
            Kind::Address => "address",
        }
    }

    fn strength(self) -> Strength {
        match self {
            Kind::PaymentCard
            | Kind::Aadhaar
            | Kind::Iban
            | Kind::Pan
            | Kind::Ssn
            | Kind::Email
            | Kind::IpAddress => Strength::Strict,
            Kind::Phone => Strength::Shaped,
            Kind::DateOfBirth | Kind::Name | Kind::Address => Strength::NameLed,
        }
    }

    /// Whether a column of `column_type` can hold this kind at all.
    fn fits(self, column_type: ColumnType) -> bool {
        match column_type {
            ColumnType::Text => true,
            ColumnType::Integer => matches!(
                self,
                Kind::Phone | Kind::PaymentCard | Kind::Aadhaar | Kind::Ssn
            ),
            ColumnType::Date => self == Kind::DateOfBirth,
        }
    }

    fn matches(self, value: &str, hinted: bool, current_year: i64) -> bool {
        match self {
            Kind::Email => values::is_email(value),
            Kind::Phone => values::is_phone(value, hinted),
            Kind::PaymentCard => values::is_payment_card(value),
            Kind::IpAddress => values::is_ip_address(value),
            Kind::Pan => values::is_pan(value),
            Kind::Aadhaar => values::is_aadhaar(value),
            Kind::Ssn => values::is_ssn(value, hinted),
            Kind::Iban => values::is_iban(value),
            Kind::DateOfBirth => values::is_birth_date(value, current_year),
            Kind::Name => values::has_name_shape(value),
            Kind::Address => values::has_address_shape(value),
        }
    }

    /// `value` with too little left to identify anyone.
    pub fn mask(self, value: &str) -> String {
        match self {
            Kind::Email => values::masked_email(value),
            Kind::IpAddress => values::masked_ip(value),
            Kind::DateOfBirth => values::masked_digits(value),
            Kind::Name | Kind::Address => values::initials(value),
            Kind::PaymentCard | Kind::Aadhaar | Kind::Ssn | Kind::Iban => {
                values::keep_last(value, 4)
            }
            Kind::Phone | Kind::Pan => values::keep_last(value, 2),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    fn raised(self) -> Self {
        match self {
            Confidence::Low => Confidence::Medium,
            Confidence::Medium | Confidence::High => Confidence::High,
        }
    }
}

/// What the values of a column are, as far as a scan cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Text,
    Integer,
    Date,
}

impl ColumnType {
    /// `None` for types no personal data kind is stored as (booleans,
    /// floating point, binary, nested types).
    pub fn of(data_type: &str) -> Option<Self> {
        let lower = data_type.trim().to_lowercase();
        let base = lower.split('(').next().unwrap_or_default();
        match base {
            "string" | "varchar" | "char" | "text" => Some(Self::Text),
            "int" | "integer" | "long" | "bigint" | "decimal" | "numeric" => Some(Self::Integer),
            "date" | "timestamp" | "timestamp_ntz" | "timestamptz" => Some(Self::Date),
            _ => None,
        }
    }
}

/// A column judged to hold one kind of personal data.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub kind: Kind,
    pub matched: usize,
    pub values: usize,
    pub confidence: Confidence,
    /// The first matching value, masked.
    pub example: String,
}

impl Finding {
    pub fn match_rate(&self) -> f64 {
        self.matched as f64 / self.values as f64
    }
}

/// The kinds a column's name points at.
pub fn hinted_kinds(column: &str) -> Vec<Kind> {
    let words = name_words(column);
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let mut kinds = Vec::new();
    let mut hint = |kind: Kind, yes: bool| {
        if yes {
            kinds.push(kind);
        }
    };
    hint(
        Kind::Email,
        has(&["email", "mail", "emailid", "emailaddress"]),
    );
    hint(
        Kind::Phone,
        has(&[
            "phone",
            "mobile",
            "cell",
            "tel",
            "telephone",
            "msisdn",
            "whatsapp",
            "contactno",
        ]),
    );
    hint(
        Kind::PaymentCard,
        has(&["card", "cardnumber", "ccnumber", "creditcard", "debitcard"]),
    );
    hint(
        Kind::IpAddress,
        has(&["ip", "ipaddress", "ipaddr", "ipv4", "ipv6"]),
    );
    hint(Kind::Pan, has(&["pan", "pannumber", "panno"]));
    hint(Kind::Aadhaar, has(&["aadhaar", "aadhar", "uid", "uidai"]));
    hint(
        Kind::Ssn,
        has(&["ssn", "socialsecurity"]) || has(&["social"]) && has(&["security"]),
    );
    hint(Kind::Iban, has(&["iban"]));
    hint(
        Kind::DateOfBirth,
        has(&["dob", "birth", "birthday", "birthdate", "bday"]),
    );
    let organisation = has(&ORGANISATION_WORDS);
    hint(Kind::Name, !organisation && is_person_name_column(&words));
    hint(
        Kind::Address,
        !organisation
            && has(&[
                "address",
                "addr",
                "street",
                "addressline",
                "line1",
                "line2",
                "address1",
                "address2",
            ]),
    );
    kinds
}

/// Words that make a name or address column an organisation's, not a
/// person's (`Entity_Legal_Name`, `supplier_address`).
const ORGANISATION_WORDS: [&str; 24] = [
    "entity",
    "company",
    "org",
    "organisation",
    "organization",
    "business",
    "supplier",
    "vendor",
    "store",
    "plant",
    "branch",
    "bank",
    "hospital",
    "facility",
    "firm",
    "merchant",
    "parent",
    "product",
    "part",
    "item",
    "brand",
    "equipment",
    "machine",
    "warehouse",
];

/// `name` with a word saying it names a person, not a product or a file.
fn is_person_name_column(words: &[String]) -> bool {
    const WHOLE: [&str; 6] = [
        "firstname",
        "lastname",
        "fullname",
        "surname",
        "givenname",
        "middlename",
    ];
    const PERSON: [&str; 33] = [
        "first",
        "last",
        "full",
        "given",
        "family",
        "middle",
        "customer",
        "contact",
        "employee",
        "person",
        "patient",
        "user",
        "owner",
        "holder",
        "cardholder",
        "account",
        "display",
        "legal",
        "maiden",
        "father",
        "mother",
        "spouse",
        "nominee",
        "beneficiary",
        "hcp",
        "doctor",
        "physician",
        "agent",
        "member",
        "applicant",
        "insured",
        "student",
        "guardian",
    ];
    if words.iter().any(|word| WHOLE.contains(&word.as_str())) {
        return true;
    }
    let names_something = words.iter().any(|word| word == "name");
    let others: Vec<&String> = words.iter().filter(|word| *word != "name").collect();
    names_something
        && (others.is_empty() || others.iter().any(|word| PERSON.contains(&word.as_str())))
}

/// `customerEmail`, `customer_email` and `Customer Email` all give
/// `["customer", "email"]`.
fn name_words(column: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for c in column.chars() {
        if !c.is_alphanumeric() {
            push_word(&mut words, &mut current);
            previous_lower = false;
            continue;
        }
        if c.is_uppercase() && previous_lower {
            push_word(&mut words, &mut current);
        }
        previous_lower = c.is_lowercase() || c.is_ascii_digit();
        current.extend(c.to_lowercase());
    }
    push_word(&mut words, &mut current);
    words
}

fn push_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

/// The kind of personal data the column most likely holds, if any.
/// `values` are its non-empty sampled values as text.
pub fn classify(
    column: &str,
    column_type: ColumnType,
    values: &[String],
    current_year: i64,
) -> Option<Finding> {
    if values.is_empty() {
        return None;
    }
    let hinted = hinted_kinds(column);
    KINDS
        .iter()
        .filter(|kind| kind.fits(column_type))
        .filter(|kind| kind.strength() != Strength::NameLed || hinted.contains(kind))
        .filter_map(|kind| judge(*kind, hinted.contains(kind), values, current_year))
        .max_by(|a, b| {
            // Highest rate wins; on a tie the earlier (stricter) kind does.
            a.match_rate()
                .partial_cmp(&b.match_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(std::cmp::Ordering::Greater)
        })
}

fn judge(kind: Kind, hinted: bool, values: &[String], current_year: i64) -> Option<Finding> {
    let mut matching = values
        .iter()
        .filter(|value| kind.matches(value.trim(), hinted, current_year));
    let example = kind.mask(matching.next()?.trim());
    let matched = 1 + matching.count();
    let rate = matched as f64 / values.len() as f64;
    let needed = if hinted { HINTED_FLAG_RATE } else { FLAG_RATE };
    if rate < needed {
        return None;
    }
    Some(Finding {
        kind,
        matched,
        values: values.len(),
        confidence: confidence(kind, hinted, rate),
        example,
    })
}

fn confidence(kind: Kind, hinted: bool, rate: f64) -> Confidence {
    let base = match kind.strength() {
        Strength::Strict if rate >= CONVINCING_RATE => Confidence::High,
        Strength::Strict if rate >= FLAG_RATE => Confidence::Medium,
        Strength::Shaped if rate >= CONVINCING_RATE => Confidence::Medium,
        Strength::NameLed if rate >= LOOSE_SHAPE_RATE => Confidence::Medium,
        _ => Confidence::Low,
    };
    let raised = if hinted { base.raised() } else { base };
    // A name or address is never certain from its shape.
    if matches!(kind, Kind::Name | Kind::Address) {
        return raised.min(Confidence::Medium);
    }
    raised
}

#[cfg(test)]
mod tests {
    use super::*;

    const YEAR: i64 = 2026;

    fn column(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn splits_column_names_into_words() {
        assert_eq!(name_words("customerEmail"), ["customer", "email"]);
        assert_eq!(
            name_words("Customer_EMAIL addr"),
            ["customer", "email", "addr"]
        );
        assert_eq!(name_words("line1"), ["line1"]);
    }

    #[test]
    fn product_and_file_names_are_not_person_names() {
        assert!(hinted_kinds("customer_name").contains(&Kind::Name));
        assert!(hinted_kinds("FirstName").contains(&Kind::Name));
        assert!(hinted_kinds("name").contains(&Kind::Name));
        assert!(!hinted_kinds("product_name").contains(&Kind::Name));
        assert!(!hinted_kinds("file_name").contains(&Kind::Name));
        assert!(!hinted_kinds("Entity_Legal_Name").contains(&Kind::Name));
        assert!(hinted_kinds("legal_name").contains(&Kind::Name));
        assert!(hinted_kinds("HCP_Name").contains(&Kind::Name));
    }

    #[test]
    fn organisation_addresses_are_not_personal() {
        assert!(hinted_kinds("customer_address").contains(&Kind::Address));
        assert!(!hinted_kinds("supplier_address").contains(&Kind::Address));
        assert!(!hinted_kinds("Entity_Address").contains(&Kind::Address));
    }

    #[test]
    fn a_column_of_emails_is_high_confidence() {
        let values = column(&["a@x.io", "b@y.com", "c@z.in", "d@w.org"]);
        let found = classify("contact", ColumnType::Text, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::Email);
        assert_eq!(found.confidence, Confidence::High);
        assert_eq!(found.example, "a***@x***.io");
    }

    #[test]
    fn a_minority_of_matches_is_not_flagged_without_a_hint() {
        let values = column(&["a@x.io", "none", "n/a", "unknown"]);
        assert!(classify("notes", ColumnType::Text, &values, YEAR).is_none());
        let found = classify("email", ColumnType::Text, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::Email);
        assert_eq!(found.confidence, Confidence::Medium);
    }

    #[test]
    fn random_ids_rarely_pass_luhn_so_they_are_not_cards() {
        let ids: Vec<String> = (0..100u64)
            .map(|i| format!("{}", 4_000_000_000_000_000 + i * 7_919))
            .collect();
        assert!(classify("order_ref", ColumnType::Text, &ids, YEAR).is_none());
    }

    #[test]
    fn integer_phone_numbers_need_the_column_name() {
        let values = column(&["9876543210", "9123456780", "8765432109"]);
        assert!(classify("ref", ColumnType::Integer, &values, YEAR).is_none());
        let found = classify("mobile", ColumnType::Integer, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::Phone);
        assert_eq!(found.confidence, Confidence::High);
        assert_eq!(found.example, "********10");
    }

    #[test]
    fn dates_are_birth_dates_only_when_the_name_says_so() {
        let values = column(&["1990-04-17", "1985-12-01", "2001-07-30"]);
        assert!(classify("production_date", ColumnType::Date, &values, YEAR).is_none());
        let found = classify("dob", ColumnType::Date, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::DateOfBirth);
        assert_eq!(found.confidence, Confidence::High);
        assert_eq!(found.example, "****-**-**");
    }

    #[test]
    fn names_stay_at_medium_confidence_at_most() {
        let values = column(&["Asha Kumar", "Ravi Shah", "Mary Jones"]);
        let found = classify("customer_name", ColumnType::Text, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::Name);
        assert_eq!(found.confidence, Confidence::Medium);
        assert_eq!(found.example, "A*** K***");
        assert!(classify("product_name", ColumnType::Text, &values, YEAR).is_none());
    }

    #[test]
    fn checksummed_kinds_win_a_tie() {
        let values = column(&["2345 6789 0124"]);
        let found = classify("doc", ColumnType::Text, &values, YEAR).unwrap();
        assert_eq!(found.kind, Kind::Aadhaar);
        assert_eq!(found.example, "**** **** 0124");
    }

    #[test]
    fn column_types_by_name() {
        assert_eq!(ColumnType::of("varchar(20)"), Some(ColumnType::Text));
        assert_eq!(ColumnType::of("decimal(18,0)"), Some(ColumnType::Integer));
        assert_eq!(ColumnType::of("timestamp"), Some(ColumnType::Date));
        assert_eq!(ColumnType::of("double"), None);
        assert_eq!(ColumnType::of("struct<a:int>"), None);
    }
}
