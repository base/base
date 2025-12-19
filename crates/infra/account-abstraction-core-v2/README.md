# account-abstraction-core-v2

Clean architecture implementation for ERC-4337 account abstraction mempool and validation.

## Architecture Overview

This crate follows **Clean Architecture** (also known as Hexagonal Architecture or Ports & Adapters). The goal is to keep business logic independent of external concerns like databases, message queues, or RPC providers.

**Note**: We use the term "interfaces" for what Hexagonal Architecture traditionally calls "ports" - both refer to the same concept of defining contracts between layers.

```
┌─────────────────────────────────────────────────────────────┐
│                        Factories                             │
│            (Wiring/Dependency Injection)                     │
└───────────────────────┬─────────────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────────────┐
│                    Infrastructure                            │
│         (Kafka, RPC providers, external systems)             │
│                  implements ▼                                │
└───────────────────────┬─────────────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────────────┐
│                      Services                                │
│              (Orchestration & use cases)                     │
│                 defines ▼ interfaces                         │
└───────────────────────┬─────────────────────────────────────┘
                        │
┌───────────────────────▼─────────────────────────────────────┐
│                       Domain                                 │
│          (Pure business logic - no dependencies)             │
└─────────────────────────────────────────────────────────────┘
```

### Dependency Direction

**Critical Rule**: Dependencies always point inward.
- ✅ `infrastructure/` depends on `services/` and `domain/`
- ✅ `services/` depends on `domain/`
- ✅ `domain/` depends on nothing
- ❌ Never reverse these dependencies

## Layer Descriptions

### 📦 Domain (`src/domain/`)

**What it is**: Pure business logic with zero external dependencies.

**Contains**:
- Core types (`UserOperation`, `ValidationResult`, `WrappedUserOperation`)
- Business events (`MempoolEvent` - what happened in our system)
- Business rules (`Mempool` trait, entrypoint validation logic)
- Domain services (in-memory mempool implementation)

**Rules**:
- No imports from `infrastructure/`, `services/`, or external crates like `rdkafka`
- Should be reusable in any context (CLI tools, web servers, tests)
- Changes here affect the entire system

**Example**:
```rust
// domain/events.rs - describes what happens in our system
pub enum MempoolEvent {
    UserOpAdded { user_op: WrappedUserOperation },
    UserOpIncluded { user_op: WrappedUserOperation },
    UserOpDropped { user_op: WrappedUserOperation, reason: String },
}
```

### 🎯 Services (`src/services/`)

**What it is**: High-level orchestration that coordinates domain logic to accomplish specific goals.

**Contains**:
- Use case implementations (`MempoolEngine` - handles mempool events)
- **Interfaces** (contracts that infrastructure must implement)

**Purpose**:
- Reusable across different binaries (ingress-rpc, batch-processor, CLI tools)
- Defines "what we need" without specifying "how we get it"
- Orchestrates domain objects to perform complex operations

**Example**:
```rust
// services/mempool_engine.rs
pub struct MempoolEngine {
    mempool: Arc<RwLock<MempoolImpl>>,
    event_source: Arc<dyn EventSource>, // ← uses an interface, not Kafka directly
}

impl MempoolEngine {
    pub async fn run(&self) {
        loop {
            let event = self.event_source.receive().await?; // ← generic!
            self.handle_event(event).await?;
        }
    }
}
```

### 🔌 Interfaces (`src/services/interfaces/`)

**What they are**: Traits that define what the services layer needs from the outside world.

**Why they exist**:
- **Dependency Inversion**: Services define what they need; infrastructure provides it
- **Testability**: Easy to mock interfaces with fake implementations
- **Flexibility**: Swap Kafka for Redis without touching service code

**Interfaces in this crate**:
- `EventSource` - "I need a stream of MempoolEvents" (Kafka? Redis? In-memory? Don't care!)
- `UserOperationValidator` - "I need to validate user operations" (RPC? Mock? Don't care!)

**Example**:
```rust
// services/interfaces/event_source.rs
#[async_trait]
pub trait EventSource: Send + Sync {
    async fn receive(&self) -> anyhow::Result<MempoolEvent>;
}

// Now we can implement this for ANY event source:
// - KafkaEventSource
// - RedisEventSource
// - MockEventSource (for tests)
// - FileEventSource
```

### 🏗️ Infrastructure (`src/infrastructure/`)

**What it is**: Adapters that connect our system to external services.

**Contains**:
- Kafka consumer (`KafkaEventSource` implements `EventSource`)
- RPC validators (`BaseNodeValidator` implements `UserOperationValidator`)
- Database clients (when needed)
- External API clients

**Purpose**:
- Translate between external systems and our domain
- Handle external concerns (serialization, retries, connection pooling)
- Implement the interfaces defined by services

**Example**:
```rust
// infrastructure/kafka/consumer.rs
pub struct KafkaEventSource {
    consumer: Arc<StreamConsumer>, // ← Kafka-specific!
}

#[async_trait]
impl EventSource for KafkaEventSource { // ← Implements the interface
    async fn receive(&self) -> anyhow::Result<MempoolEvent> {
        let msg = self.consumer.recv().await?.detach();
        let payload = msg.payload().ok_or(...)?;
        let event: MempoolEvent = serde_json::from_slice(payload)?;
        Ok(event)
    }
}
```

### 🏭 Factories (`src/factories/`)

**What they are**: Convenience functions that wire everything together.

**Contains**:
- `create_mempool_engine()` - creates a fully-wired MempoolEngine with Kafka consumer

**Purpose**:
- Reduce boilerplate in main.rs
- Provide sensible defaults
- Make it easy to get started

**When to use**:
- Quick setup in binaries
- Standard configurations

**When NOT to use**:
- Custom wiring needed
- Testing (inject mocks directly)
- Non-standard configurations

**Example**:
```rust
// factories/kafka_engine.rs
pub fn create_mempool_engine(
    properties_file: &str,
    topic: &str,
    consumer_group_id: &str,
    pool_config: Option<PoolConfig>,
) -> anyhow::Result<Arc<MempoolEngine>> {
    // 1. Create Kafka consumer (infrastructure)
    let consumer: StreamConsumer = create_kafka_consumer(...)?;

    // 2. Wrap in interface adapter
    let event_source = Arc::new(KafkaEventSource::new(Arc::new(consumer)));

    // 3. Create service with interface
    let engine = MempoolEngine::with_event_source(event_source, pool_config);

    Ok(Arc::new(engine))
}
```

## Why This Architecture?

### ✅ Benefits

1. **Testability**: Mock interfaces instead of real Kafka/RPC
   ```rust
   // Test with fake event source
   let mock_source = Arc::new(MockEventSource::new(vec![event1, event2]));
   let engine = MempoolEngine::with_event_source(mock_source, None);
   ```

2. **Flexibility**: Swap infrastructure without touching business logic
   ```rust
   // Production: Kafka
   let source = KafkaEventSource::new(kafka_consumer);

   // Development: In-memory
   let source = InMemoryEventSource::new(vec![...]);

   // Same engine works with both!
   let engine = MempoolEngine::with_event_source(source, config);
   ```

3. **Reusability**: Services can be used in multiple binaries
   ```rust
   // ingress-rpc binary
   use account_abstraction_core_v2::MempoolEngine;

   // batch-processor binary
   use account_abstraction_core_v2::MempoolEngine;

   // Same code, different contexts!
   ```

4. **Clear boundaries**: Each layer has a single responsibility
   - Domain: Business rules
   - Services: Orchestration
   - Infrastructure: External systems
   - Factories: Wiring

5. **Independent evolution**: Change infrastructure without affecting domain
   - Migrate Kafka → Redis: Only touch `infrastructure/`
   - Add new validation rule: Only touch `domain/`
   - Change orchestration: Only touch `services/`

## Usage Examples

### Basic Usage (with Factory)

```rust
use account_abstraction_core_v2::create_mempool_engine;

let engine = create_mempool_engine(
    "kafka.properties",
    "user-operations",
    "mempool-consumer",
    None,
)?;

tokio::spawn(async move {
    engine.run().await;
});
```

### Custom Setup (without Factory)

```rust
use account_abstraction_core_v2::{
    MempoolEngine,
    infrastructure::kafka::consumer::KafkaEventSource,
};

let kafka_consumer = create_kafka_consumer(...)?;
let event_source = Arc::new(KafkaEventSource::new(Arc::new(kafka_consumer)));
let engine = MempoolEngine::with_event_source(event_source, Some(custom_config));
```

### Testing

```rust
use account_abstraction_core_v2::{
    MempoolEngine, MempoolEvent,
    services::interfaces::event_source::EventSource,
};

struct MockEventSource {
    events: Vec<MempoolEvent>,
}

#[async_trait]
impl EventSource for MockEventSource {
    async fn receive(&self) -> anyhow::Result<MempoolEvent> {
        // Return test events
    }
}

let mock = Arc::new(MockEventSource { events: test_events });
let engine = MempoolEngine::with_event_source(mock, None);
// Test without any real Kafka!
```

## Further Reading

- [Clean Architecture (Robert C. Martin)](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html)
- [Hexagonal Architecture](https://alistair.cockburn.us/hexagonal-architecture/)
- [Ports and Adapters Pattern](https://jmgarridopaz.github.io/content/hexagonalarchitecture.html)
