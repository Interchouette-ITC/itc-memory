SELECT count(*) FROM indexing_failures f JOIN indexing_runs r ON r.id=f.indexing_run_id WHERE f.path LIKE '%src/broken.rs' AND r.status='partial'
