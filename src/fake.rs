//! Made-up values that look real: names, emails, card numbers and so on. Each value comes from a
//! seed, and the same seed always makes the same value.
//!
//! None of them can be a real person's: SSNs use areas never issued, cards use a test range,
//! phone numbers use the fictional 555-01XX range, emails use the reserved `example` domains, and
//! IPs come from the documentation networks.

use std::fmt::{self, Display, Formatter, Write};
use std::str::FromStr;

/// A kind of made-up value, as `fake:` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fake {
    /// `Maria`.
    FirstName,
    /// `Lopez`.
    LastName,
    /// `Maria Lopez`.
    Name,
    /// `maria.lopez.3j9x0a2b1c4d@example.com`. The suffix keeps unique indexes happy.
    Email,
    /// `maria_3j9x0a2b1c4d`.
    Username,
    /// `415-555-0137`.
    Phone,
    /// `912345678`: nine digits, in an area (900–999) the SSA never issues.
    Ssn,
    /// `4000001234567899`: sixteen digits that pass the Luhn check, in a test range.
    CreditCard,
    /// `1234 Maple Ave`.
    StreetAddress,
    /// `Apt. 12`.
    SecondaryAddress,
    /// `Springfield`.
    City,
    /// `OR`.
    State,
    /// `97201`.
    Zip,
    /// `Lopez Group`.
    Company,
    /// `1978-04-23`, between 1945 and 2005.
    DateOfBirth,
    /// `192.0.2.17`, from the documentation networks.
    Ip,
    /// A short sentence of lorem ipsum.
    Lorem,
}

impl Fake {
    /// Every kind, in the order the docs list them.
    pub const ALL: [Self; 17] = [
        Self::FirstName,
        Self::LastName,
        Self::Name,
        Self::Email,
        Self::Username,
        Self::Phone,
        Self::Ssn,
        Self::CreditCard,
        Self::StreetAddress,
        Self::SecondaryAddress,
        Self::City,
        Self::State,
        Self::Zip,
        Self::Company,
        Self::DateOfBirth,
        Self::Ip,
        Self::Lorem,
    ];

    /// Returns the name `fake:` uses for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FirstName => "first_name",
            Self::LastName => "last_name",
            Self::Name => "name",
            Self::Email => "email",
            Self::Username => "username",
            Self::Phone => "phone",
            Self::Ssn => "ssn",
            Self::CreditCard => "credit_card",
            Self::StreetAddress => "street_address",
            Self::SecondaryAddress => "secondary_address",
            Self::City => "city",
            Self::State => "state",
            Self::Zip => "zip",
            Self::Company => "company",
            Self::DateOfBirth => "date_of_birth",
            Self::Ip => "ip",
            Self::Lorem => "lorem",
        }
    }

    /// Returns whether every value is only digits, so it fits a numeric column.
    #[must_use]
    pub fn is_digits(self) -> bool {
        match self {
            Self::Ssn | Self::CreditCard => true,
            Self::FirstName
            | Self::LastName
            | Self::Name
            | Self::Email
            | Self::Username
            | Self::Phone
            | Self::StreetAddress
            | Self::SecondaryAddress
            | Self::City
            | Self::State
            | Self::Zip
            | Self::Company
            | Self::DateOfBirth
            | Self::Ip
            | Self::Lorem => false,
        }
    }

    /// Appends the value `seed` makes to `out`. Values are plain ASCII, without quotes or
    /// backslashes.
    pub fn write(self, seed: u128, out: &mut String) {
        // Writing to a `String` can't fail.
        let _ = self.try_write(seed, out);
    }

    fn try_write(self, seed: u128, out: &mut String) -> fmt::Result {
        let mut draws = Draws::new(seed);
        // The high half of the seed, untouched by the draws, keeps emails and usernames unique.
        let unique = Base36((seed >> 64) as u64);
        match self {
            Self::FirstName => out.push_str(draws.pick(FIRST_NAMES)),
            Self::LastName => out.push_str(draws.pick(LAST_NAMES)),
            Self::Name => {
                let first = draws.pick(FIRST_NAMES);
                let last = draws.pick(LAST_NAMES);
                write!(out, "{first} {last}")?;
            }
            Self::Email => {
                let first = draws.pick(FIRST_NAMES).to_ascii_lowercase();
                let last = draws.pick(LAST_NAMES).to_ascii_lowercase();
                let domain = draws.pick(&["com", "net", "org"]);
                write!(out, "{first}.{last}.{unique}@example.{domain}")?;
            }
            Self::Username => {
                let first = draws.pick(FIRST_NAMES).to_ascii_lowercase();
                write!(out, "{first}_{unique}")?;
            }
            Self::Phone => {
                let area = draws.range(201, 989);
                let line = draws.below(100);
                write!(out, "{area}-555-01{line:02}")?;
            }
            Self::Ssn => {
                let area = draws.range(900, 999);
                let group = draws.range(1, 99);
                let serial = draws.range(1, 9999);
                write!(out, "{area}{group:02}{serial:04}")?;
            }
            Self::CreditCard => {
                let mut digits = [0u8; 16];
                digits[..6].copy_from_slice(&[4, 0, 0, 0, 0, 0]);
                for digit in &mut digits[6..15] {
                    *digit = draws.below(10) as u8;
                }
                digits[15] = luhn_check_digit(&digits[..15]);
                for digit in digits {
                    out.push(char::from(b'0' + digit));
                }
            }
            Self::StreetAddress => {
                let number = draws.range(1, 9999);
                let street = draws.pick(STREETS);
                let suffix = draws.pick(STREET_SUFFIXES);
                write!(out, "{number} {street} {suffix}")?;
            }
            Self::SecondaryAddress => {
                let unit = draws.pick(&["Apt.", "Suite", "Unit"]);
                let number = draws.range(1, 999);
                write!(out, "{unit} {number}")?;
            }
            Self::City => out.push_str(draws.pick(CITIES)),
            Self::State => out.push_str(draws.pick(STATES)),
            Self::Zip => write!(out, "{:05}", draws.range(501, 99_950))?,
            Self::Company => {
                let name = draws.pick(LAST_NAMES);
                let suffix = draws.pick(COMPANY_SUFFIXES);
                write!(out, "{name} {suffix}")?;
            }
            Self::DateOfBirth => {
                let year = draws.range(1945, 2005);
                let month = draws.range(1, 12);
                let day = draws.range(1, 28);
                write!(out, "{year}-{month:02}-{day:02}")?;
            }
            Self::Ip => {
                let network = draws.pick(&["192.0.2", "198.51.100", "203.0.113"]);
                write!(out, "{network}.{}", draws.range(1, 254))?;
            }
            Self::Lorem => {
                let words = draws.range(6, 12);
                for index in 0..words {
                    let word = draws.pick(LOREM);
                    if index == 0 {
                        let mut letters = word.chars();
                        if let Some(first) = letters.next() {
                            out.push(first.to_ascii_uppercase());
                            out.push_str(letters.as_str());
                        }
                    } else {
                        out.push(' ');
                        out.push_str(word);
                    }
                }
                out.push('.');
            }
        }
        Ok(())
    }
}

impl Display for Fake {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Fake {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if let Some(&fake) = Self::ALL.iter().find(|fake| fake.as_str() == name) {
            return Ok(fake);
        }
        let names: Vec<&str> = Self::ALL.iter().map(|fake| fake.as_str()).collect();
        Err(format!(
            "`{name}` isn't a fake; use one of {}",
            names.join(", ")
        ))
    }
}

/// Numbers drawn from a seed with splitmix64: fast, and good enough to pick from lists.
struct Draws {
    state: u64,
}

impl Draws {
    fn new(seed: u128) -> Self {
        Self { state: seed as u64 }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    /// Returns a number from 0 to `count - 1`.
    fn below(&mut self, count: u64) -> u64 {
        self.next() % count
    }

    /// Returns a number from `low` to `high`, both included.
    fn range(&mut self, low: u64, high: u64) -> u64 {
        low + self.below(high - low + 1)
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

/// Writes a number in lowercase base 36: at most 13 characters for a `u64`.
struct Base36(u64);

impl Display for Base36 {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let mut buffer = [0u8; 13];
        let mut start = buffer.len();
        let mut rest = self.0;
        loop {
            start -= 1;
            buffer[start] = DIGITS[(rest % 36) as usize];
            rest /= 36;
            if rest == 0 {
                break;
            }
        }
        for &digit in &buffer[start..] {
            f.write_char(char::from(digit))?;
        }
        Ok(())
    }
}

/// Returns the digit that makes `payload` followed by it pass the Luhn check.
fn luhn_check_digit(payload: &[u8]) -> u8 {
    let mut sum = 0u32;
    // Counting from the right, where the check digit goes, every second payload digit doubles,
    // starting with the last one.
    for (index, &digit) in payload.iter().rev().enumerate() {
        let mut value = u32::from(digit);
        if index % 2 == 0 {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
    }
    ((10 - sum % 10) % 10) as u8
}

const FIRST_NAMES: &[&str] = &[
    "Ada", "Alex", "Amara", "Ana", "Ben", "Bianca", "Carlos", "Chen", "Chloe", "Daniel", "Diego",
    "Elena", "Emma", "Farah", "Grace", "Hana", "Ivan", "Jamal", "Jane", "Jonas", "Kai", "Lena",
    "Leo", "Lucia", "Maria", "Mateo", "Maya", "Nia", "Noah", "Omar", "Priya", "Rafael", "Rosa",
    "Sam", "Sofia", "Tariq", "Uma", "Victor", "Wei", "Yara", "Zoe",
];

const LAST_NAMES: &[&str] = &[
    "Adams", "Baker", "Carter", "Chen", "Costa", "Diaz", "Evans", "Fischer", "Garcia", "Hall",
    "Hughes", "Ito", "Jensen", "Kim", "Kowalski", "Lee", "Lopez", "Martin", "Meyer", "Moreau",
    "Nakamura", "Nguyen", "Novak", "Okafor", "Patel", "Perez", "Quinn", "Reyes", "Rossi", "Santos",
    "Schmidt", "Silva", "Singh", "Smith", "Tanaka", "Torres", "Walker", "Weber", "Young", "Zhang",
];

const STREETS: &[&str] = &[
    "Maple",
    "Oak",
    "Pine",
    "Cedar",
    "Elm",
    "Birch",
    "Willow",
    "Lake",
    "Hill",
    "River",
    "Park",
    "Sunset",
    "Ridge",
    "Meadow",
    "Forest",
    "Spring",
    "Valley",
    "Harbor",
    "Mill",
    "Church",
    "Washington",
    "Lincoln",
    "Jefferson",
    "Franklin",
    "Highland",
    "Orchard",
    "Prospect",
    "Chestnut",
    "Walnut",
    "Market",
];

const STREET_SUFFIXES: &[&str] = &["St", "Ave", "Rd", "Ln", "Dr", "Ct", "Blvd", "Way", "Pl"];

const CITIES: &[&str] = &[
    "Springfield",
    "Riverside",
    "Fairview",
    "Franklin",
    "Greenville",
    "Bristol",
    "Clinton",
    "Georgetown",
    "Salem",
    "Madison",
    "Arlington",
    "Ashland",
    "Burlington",
    "Chester",
    "Dayton",
    "Dover",
    "Hudson",
    "Kingston",
    "Lexington",
    "Milton",
    "Newport",
    "Oxford",
    "Plymouth",
    "Richmond",
    "Shelby",
    "Troy",
    "Winchester",
];

const STATES: &[&str] = &[
    "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS",
    "KY", "LA", "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY",
    "NC", "ND", "OH", "OK", "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV",
    "WI", "WY",
];

const COMPANY_SUFFIXES: &[&str] = &[
    "Group", "Partners", "Labs", "Works", "Holdings", "Supply", "Services", "Studio",
];

const LOREM: &[&str] = &[
    "lorem",
    "ipsum",
    "dolor",
    "sit",
    "amet",
    "consectetur",
    "adipiscing",
    "elit",
    "sed",
    "do",
    "eiusmod",
    "tempor",
    "incididunt",
    "ut",
    "labore",
    "et",
    "dolore",
    "magna",
    "aliqua",
    "enim",
    "ad",
    "minim",
    "veniam",
    "quis",
    "nostrud",
    "exercitation",
    "ullamco",
    "laboris",
    "nisi",
    "aliquip",
    "ex",
    "ea",
    "commodo",
    "consequat",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn value(fake: Fake, seed: u128) -> String {
        let mut out = String::new();
        fake.write(seed, &mut out);
        out
    }

    /// Seeds spread over the whole range, as hashes are.
    fn seeds() -> impl Iterator<Item = u128> {
        (1..=500u128).map(|index| index.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835))
    }

    fn passes_luhn(digits: &str) -> bool {
        let (payload, check) = digits.split_at(digits.len() - 1);
        let payload: Vec<u8> = payload.bytes().map(|byte| byte - b'0').collect();
        check.as_bytes()[0] - b'0' == luhn_check_digit(&payload)
    }

    #[test]
    fn parses_and_names_every_kind() {
        for fake in Fake::ALL {
            assert_eq!(fake.as_str().parse::<Fake>(), Ok(fake));
            assert_eq!(fake.to_string(), fake.as_str());
        }
        let error = "e-mail".parse::<Fake>().expect_err("not a kind");
        assert!(
            error.starts_with("`e-mail` isn't a fake; use one of first_name, last_name, name,"),
            "{error}"
        );
    }

    #[test]
    fn the_same_seed_makes_the_same_value() {
        for fake in Fake::ALL {
            let seed = 0x1234_5678_9abc_def0_0fed_cba9_8765_4321;
            assert_eq!(value(fake, seed), value(fake, seed), "{fake}");
            let distinct: std::collections::HashSet<String> =
                seeds().map(|seed| value(fake, seed)).collect();
            assert!(distinct.len() > 1, "{fake} always makes {distinct:?}");
        }
    }

    #[test]
    fn values_are_plain_ascii_that_needs_no_escaping() {
        for fake in Fake::ALL {
            for seed in seeds() {
                let made = value(fake, seed);
                assert!(
                    !made.is_empty()
                        && made
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() || byte == b' '),
                    "{fake} made {made:?}"
                );
                assert!(
                    !made.contains(['\'', '"', '\\']),
                    "{fake} made {made:?}, which needs escaping"
                );
                if fake.is_digits() {
                    assert!(
                        made.bytes().all(|byte| byte.is_ascii_digit()),
                        "{fake}: {made}"
                    );
                }
            }
        }
    }

    #[test]
    fn ssns_are_never_issued_ones() {
        for seed in seeds() {
            let ssn = value(Fake::Ssn, seed);
            assert_eq!(ssn.len(), 9, "{ssn}");
            let area: u32 = ssn[..3].parse().expect("digits");
            assert!(area >= 900, "{ssn} is in area {area}, which the SSA issues");
            assert_ne!(&ssn[3..5], "00", "{ssn} has group 00");
            assert_ne!(&ssn[5..], "0000", "{ssn} has serial 0000");
        }
    }

    #[test]
    fn card_numbers_pass_luhn_in_a_test_range() {
        assert!(passes_luhn("4111111111111111"), "a known test card");
        assert!(!passes_luhn("4111111111111112"), "one digit off");
        for seed in seeds() {
            let card = value(Fake::CreditCard, seed);
            assert_eq!(card.len(), 16, "{card}");
            assert!(
                card.starts_with("400000"),
                "{card} is outside the test range"
            );
            assert!(passes_luhn(&card), "{card} fails the Luhn check");
        }
    }

    #[test]
    fn emails_use_reserved_domains_and_stay_unique() {
        let mut seen = std::collections::HashSet::new();
        for seed in seeds() {
            let email = value(Fake::Email, seed);
            assert!(
                ["@example.com", "@example.net", "@example.org"]
                    .iter()
                    .any(|domain| email.ends_with(domain)),
                "{email} isn't on a reserved domain"
            );
            assert_eq!(email, email.to_ascii_lowercase());
            assert!(seen.insert(email.clone()), "{email} came up twice");
        }
    }

    #[test]
    fn phones_and_ips_are_fictional() {
        for seed in seeds() {
            let phone = value(Fake::Phone, seed);
            assert!(phone[3..].starts_with("-555-01"), "{phone}");
            let ip = value(Fake::Ip, seed);
            assert!(
                ["192.0.2.", "198.51.100.", "203.0.113."]
                    .iter()
                    .any(|network| ip.starts_with(network)),
                "{ip} isn't a documentation address"
            );
        }
    }

    #[test]
    fn dates_of_birth_are_valid_dates() {
        for seed in seeds() {
            let date = value(Fake::DateOfBirth, seed);
            let parts: Vec<u32> = date
                .split('-')
                .map(|part| part.parse().expect("digits"))
                .collect();
            assert!(
                matches!(parts[..], [1945..=2005, 1..=12, 1..=28]),
                "{date} isn't a date between 1945 and 2005"
            );
        }
    }

    #[test]
    fn writes_base36() {
        assert_eq!(Base36(0).to_string(), "0");
        assert_eq!(Base36(35).to_string(), "z");
        assert_eq!(Base36(36).to_string(), "10");
        assert_eq!(Base36(u64::MAX).to_string(), "3w5e11264sgsf");
    }
}
