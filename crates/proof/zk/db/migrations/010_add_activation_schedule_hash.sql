-- Persist the activation schedule hash so retries and outbox recovery preserve
-- the schedule binding committed into proof journals.
ALTER TABLE proof_requests ADD COLUMN activation_schedule_hash VARCHAR(66);
