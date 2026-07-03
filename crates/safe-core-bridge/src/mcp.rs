#[cfg(feature = "mcp")]
pub mod mcp_impl {
    use std::sync::Arc;

    use rmcp::{ServerHandler, ServiceExt, transport::stdio};

    use crate::{state::BridgeState, tools};

    #[derive(Clone)]
    pub struct SafeCoreMcpServer {
        state: Arc<BridgeState>,
    }

    impl SafeCoreMcpServer {
        pub fn new(state: Arc<BridgeState>) -> Self {
            Self { state }
        }

        pub async fn run_stdio(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let service = self.serve(rmcp::transport::stdio()).await?;
            service.waiting().await.map_err(|e| e.into()).map(|_| ())
        }
    }

    impl ServerHandler for SafeCoreMcpServer {}
}

#[cfg(not(feature = "mcp"))]
pub mod mcp_impl {
    pub struct SafeCoreMcpServer;

    impl SafeCoreMcpServer {
        pub fn new(_: std::sync::Arc<crate::state::BridgeState>) -> Self {
            Self
        }

        pub async fn run_stdio(self) -> Result<(), anyhow::Error> {
            Err(anyhow::anyhow!("MCP não disponível"))
        }
    }
}
