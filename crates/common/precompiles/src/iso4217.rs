//! ISO 4217 fiat-code validation for B-20 stablecoin creation.

use iso4217_static::Currency;

/// ISO 4217 helpers mirrored from `base-std`'s `ISO4217.sol`.
#[derive(Debug, Clone, Copy)]
pub struct Iso4217;

impl Iso4217 {
    /// ISO 4217 codes deliberately excluded from the stablecoin fiat allowlist.
    pub const EXCLUDED_CODES: &'static [&'static str] = &[
        "XAU", "XAG", "XPT", "XPD", "XBA", "XBB", "XBC", "XBD", "XDR", "XSU", "XUA", "XXX", "XTS",
        "BOV", "CHE", "CHW", "CLF", "COU", "MXV", "USN", "UYI", "UYW",
    ];

    /// Returns true iff `code` is a supported active ISO 4217 circulating fiat code.
    pub fn is_valid_fiat_code(code: &str) -> bool {
        Currency::try_from(code).is_ok() && Self::is_allowed_fiat_code(code)
    }

    /// Returns the number of deliberately excluded ISO 4217 codes.
    pub const fn excluded_count() -> usize {
        Self::EXCLUDED_CODES.len()
    }

    /// Returns a deliberately excluded ISO 4217 code by index.
    pub fn excluded_at(index: usize) -> Option<&'static str> {
        Self::EXCLUDED_CODES.get(index).copied()
    }

    /// Returns true iff `code` is in Base's stablecoin fiat allowlist.
    pub fn is_allowed_fiat_code(code: &str) -> bool {
        matches!(
            code.as_bytes(),
            b"USD"
                | b"UAH"
                | b"UGX"
                | b"UYU"
                | b"UZS"
                | b"EUR"
                | b"EGP"
                | b"ETB"
                | b"ERN"
                | b"JPY"
                | b"JMD"
                | b"JOD"
                | b"GBP"
                | b"GHS"
                | b"GEL"
                | b"GTQ"
                | b"GIP"
                | b"GMD"
                | b"GNF"
                | b"GYD"
                | b"CHF"
                | b"CNY"
                | b"CAD"
                | b"CZK"
                | b"COP"
                | b"CRC"
                | b"CUP"
                | b"CVE"
                | b"CDF"
                | b"AUD"
                | b"AED"
                | b"ARS"
                | b"AMD"
                | b"ANG"
                | b"AOA"
                | b"AFN"
                | b"ALL"
                | b"AWG"
                | b"AZN"
                | b"NOK"
                | b"NZD"
                | b"NGN"
                | b"NPR"
                | b"NIO"
                | b"NAD"
                | b"SEK"
                | b"SGD"
                | b"SAR"
                | b"SHP"
                | b"SCR"
                | b"SBD"
                | b"SDG"
                | b"SLE"
                | b"SOS"
                | b"SRD"
                | b"SSP"
                | b"STN"
                | b"SVC"
                | b"SYP"
                | b"SZL"
                | b"INR"
                | b"IDR"
                | b"ILS"
                | b"ISK"
                | b"IQD"
                | b"IRR"
                | b"MXN"
                | b"MYR"
                | b"MAD"
                | b"MNT"
                | b"MMK"
                | b"MUR"
                | b"MOP"
                | b"MVR"
                | b"MWK"
                | b"MGA"
                | b"MDL"
                | b"MZN"
                | b"MKD"
                | b"MRU"
                | b"TRY"
                | b"THB"
                | b"TWD"
                | b"TZS"
                | b"TND"
                | b"TOP"
                | b"TTD"
                | b"TJS"
                | b"TMT"
                | b"PLN"
                | b"PHP"
                | b"PKR"
                | b"PEN"
                | b"PGK"
                | b"PYG"
                | b"PAB"
                | b"KRW"
                | b"KZT"
                | b"KES"
                | b"KWD"
                | b"KGS"
                | b"KHR"
                | b"KMF"
                | b"KPW"
                | b"KYD"
                | b"BRL"
                | b"BHD"
                | b"BDT"
                | b"BGN"
                | b"BAM"
                | b"BBD"
                | b"BIF"
                | b"BMD"
                | b"BND"
                | b"BOB"
                | b"BSD"
                | b"BTN"
                | b"BWP"
                | b"BYN"
                | b"BZD"
                | b"HKD"
                | b"HUF"
                | b"HNL"
                | b"HTG"
                | b"RUB"
                | b"RON"
                | b"RSD"
                | b"RWF"
                | b"DKK"
                | b"DOP"
                | b"DZD"
                | b"DJF"
                | b"XOF"
                | b"XAF"
                | b"XCD"
                | b"XPF"
                | b"ZAR"
                | b"ZMW"
                | b"ZWG"
                | b"VND"
                | b"VES"
                | b"VED"
                | b"VUV"
                | b"LKR"
                | b"LBP"
                | b"LAK"
                | b"LRD"
                | b"LSL"
                | b"LYD"
                | b"FJD"
                | b"FKP"
                | b"OMR"
                | b"QAR"
                | b"WST"
                | b"YER"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Iso4217;

    #[test]
    fn accepts_base_std_fiat_codes() {
        for code in ["USD", "EUR", "JPY", "GBP", "AUD", "SGD", "XOF", "XAF", "XCD", "XPF", "ZWG"] {
            assert!(Iso4217::is_valid_fiat_code(code), "{code} should be accepted");
        }
    }

    #[test]
    fn rejects_non_allowlist_codes() {
        for code in ["", "US", "USDC", "usd", "BTC", "ETH", "ZZZ"] {
            assert!(!Iso4217::is_valid_fiat_code(code), "{code} should be rejected");
        }
    }

    #[test]
    fn rejects_base_std_excluded_codes() {
        assert_eq!(Iso4217::excluded_count(), 22);
        for index in 0..Iso4217::excluded_count() {
            let code = Iso4217::excluded_at(index).unwrap();
            assert!(!Iso4217::is_valid_fiat_code(code), "{code} should be rejected");
        }
    }
}
