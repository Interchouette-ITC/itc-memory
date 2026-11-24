SELECT EXISTS(SELECT 1 FROM indexing_failures WHERE indexing_run_id=$1)
