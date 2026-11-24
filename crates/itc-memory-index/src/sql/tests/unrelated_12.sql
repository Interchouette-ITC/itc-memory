SELECT count(*) FROM indexing_runs WHERE configuration->>'branch_changed'='true' OR configuration->>'head_changed'='true'
