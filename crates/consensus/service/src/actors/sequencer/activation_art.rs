//! ASCII art for notable sequencer activation events.

pub(super) struct ActivationArt;

impl ActivationArt {
    pub(super) const BASE_V1_ACTIVATION_BANNER: &str = r#"######################################################
#                                                    #
#  BBBBB    AAA     SSSSS  EEEEE      V   V     111  #
#  B   B   A   A   S      E          V   V    1   1  #
#  BBBBB   AAAAA    SSS   EEEE       V   V        1  #
#  B   B   A   A       S  E           V V         1  #
#  BBBBB   A   A   SSSSS  EEEEE      V        11111  #
#                                                    #
#           ALL YOUR BASE ARE BELONG TO US           #
#                                                    #
######################################################"#;
}

#[cfg(test)]
mod tests {
    use super::ActivationArt;

    #[test]
    fn base_v1_banner_is_rectangular() {
        let mut lines = ActivationArt::BASE_V1_ACTIVATION_BANNER.lines();
        let expected_width = lines.next().expect("banner must not be empty").len();
        assert!(expected_width > 0, "banner must not be empty");

        for line in lines {
            assert_eq!(line.len(), expected_width, "banner lines must stay aligned");
        }
    }
}
