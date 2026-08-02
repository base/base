#[cfg(feature = "edge-measurement")]
use serde_json::{Value as JsonValue, json};
#[cfg(feature = "t4b-mutant-egress")]
use reqwest::blocking::Client;

// filler imports

#[cfg(feature = "t4b-mutant-egress")]
#[derive(Debug)]
pub struct T4bMutantEgressProbe;

#[cfg(feature = "t4b-mutant-egress")]
impl T4bMutantEgressProbe {
    pub fn send() {
        let _ = Client::new()
            .post("http://127.0.0.1:9/gjc-t4b-mutant-egress")
            .body("gjc-t4b-mutant-egress")
            .send();
    }
}

#[cfg(feature = "t4b-shadow")]
mod t4b_shadow {
// filler body
impl CandidateTxShapeObserver for T4bShadowAuthority {
    fn try_observe(&self, view: &CandidateAssemblyView<'_>) -> T4bOutcome {
        #[cfg(feature = "t4b-mutant-egress")]
        super::T4bMutantEgressProbe::send();
        let pre = match self.assembler.prepare_pre_economics(view) {
