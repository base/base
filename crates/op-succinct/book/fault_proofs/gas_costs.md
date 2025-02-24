## Gas Costs

For the most accurate gas estimates, always test with your specific use case and conditions.

Here are the typical gas costs for key operations in the fault proof system:

### Game Creation (Propose)
Creating a new game through the factory costs approximately 330,000 gas. This includes:
- Factory proxy delegation (~25,000 gas)
- Game proxy creation (~44,000 gas)
- Game initialization (~175,000 gas)
- Event emissions and state updates

### Challenge
Challenging a game costs approximately 85,000 gas. This includes:
- State validation checks
- Game over checks
- Updating game status
- Setting challenge deadline
- Recording challenger address
- Event emissions

### Proving
Proving a game costs approximately 280,000~300,000 gas. This includes:
- Game over checks
- Proof verification
- State updates
- Event emissions

### Resolution
Resolving a game costs approximately 85,000 gas for a standard resolution. This includes:
- Parent game validation
- Status checks and updates
- Bond distribution calculations
- Event emissions

### Credit Claiming
Claiming credit (bonds/rewards) costs approximately 85,000 gas. This includes:
- Game finalization checks
- Credit balance updates
- ETH transfers
- Event emissions
