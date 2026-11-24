SELECT message FROM indexing_failures WHERE indexing_run_id=$1 ORDER BY created_at
