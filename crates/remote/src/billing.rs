/// Billing is not part of this server: every organization is unmetered.
#[derive(Clone, Default)]
pub struct BillingService;

impl BillingService {
    pub fn new() -> Self {
        Self
    }

    pub fn is_configured(&self) -> bool {
        false
    }

    /// There is never a billing provider.
    pub fn provider(&self) -> Option<std::convert::Infallible> {
        None
    }
}
