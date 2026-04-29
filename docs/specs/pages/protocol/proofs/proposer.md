# Proposer

This page will specify the proof proposer component.

The proposer is responsible for selecting canonical L2 checkpoint ranges, obtaining proof material,
and creating `AggregateVerifier` games on L1. The full proposer specification will define checkpoint
selection, parent-game selection, proof request rules, onchain submission, and retry behavior.
