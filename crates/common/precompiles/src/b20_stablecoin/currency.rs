//! ISO 4217 currency code allowlist for stablecoin B-20 tokens.

/// A validated ISO 4217 currency code from the supported allowlist.
#[derive(Debug)]
pub struct IsoCurrency;

impl IsoCurrency {
    /// Returns `Some(())` if `code` is on the supported currency allowlist, `None` otherwise.
    pub fn from_code(code: &str) -> Option<()> {
        match code {
            "AED" | "AFN" | "ALL" | "AMD" | "ANG" | "AOA" | "ARS" | "AUD" | "AWG" | "AZN"
            | "BAM" | "BBD" | "BDT" | "BGN" | "BHD" | "BIF" | "BMD" | "BND" | "BOB" | "BRL"
            | "BSD" | "BTN" | "BWP" | "BYN" | "BZD" | "CAD" | "CDF" | "CHF" | "CNY" | "COP"
            | "CRC" | "CUP" | "CVE" | "CZK" | "DJF" | "DKK" | "DOP" | "DZD" | "EGP" | "ERN"
            | "ETB" | "EUR" | "FJD" | "FKP" | "GBP" | "GEL" | "GHS" | "GIP" | "GMD" | "GNF"
            | "GTQ" | "GYD" | "HKD" | "HNL" | "HTG" | "HUF" | "IDR" | "ILS" | "INR" | "IQD"
            | "IRR" | "ISK" | "JMD" | "JOD" | "JPY" | "KES" | "KGS" | "KHR" | "KMF" | "KPW"
            | "KRW" | "KWD" | "KYD" | "KZT" | "LAK" | "LBP" | "LKR" | "LRD" | "LSL" | "LYD"
            | "MAD" | "MDL" | "MGA" | "MKD" | "MMK" | "MNT" | "MOP" | "MRU" | "MUR" | "MVR"
            | "MWK" | "MXN" | "MYR" | "MZN" | "NAD" | "NGN" | "NIO" | "NOK" | "NPR" | "NZD"
            | "OMR" | "PAB" | "PEN" | "PGK" | "PHP" | "PKR" | "PLN" | "PYG" | "QAR" | "RON"
            | "RSD" | "RUB" | "RWF" | "SAR" | "SBD" | "SCR" | "SDG" | "SEK" | "SGD" | "SHP"
            | "SLE" | "SOS" | "SRD" | "SSP" | "STN" | "SVC" | "SYP" | "SZL" | "THB" | "TJS"
            | "TMT" | "TND" | "TOP" | "TRY" | "TTD" | "TWD" | "TZS" | "UAH" | "UGX" | "USD"
            | "UYU" | "UZS" | "VED" | "VES" | "VND" | "VUV" | "WST" | "XAF" | "XCD" | "XOF"
            | "XPF" | "YER" | "ZAR" | "ZMW" | "ZWG" => Some(()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IsoCurrency;

    #[test]
    fn accepts_known_codes() {
        assert!(IsoCurrency::from_code("AED").is_some());
        assert!(IsoCurrency::from_code("USD").is_some());
        assert!(IsoCurrency::from_code("ZWG").is_some());
    }

    #[test]
    fn rejects_unknown_codes() {
        assert!(IsoCurrency::from_code("").is_none());
        assert!(IsoCurrency::from_code("usd").is_none()); // lowercase
        assert!(IsoCurrency::from_code("Usd").is_none()); // mixed case
        assert!(IsoCurrency::from_code("US").is_none()); // too short
        assert!(IsoCurrency::from_code("USDD").is_none()); // too long
        assert!(IsoCurrency::from_code("XYZ").is_none()); // plausible but not on allowlist
        assert!(IsoCurrency::from_code("BTC").is_none()); // crypto
    }
}
