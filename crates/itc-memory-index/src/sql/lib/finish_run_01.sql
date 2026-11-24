UPDATE indexing_runs SET repository_location_id=COALESCE($2,repository_location_id),status=$3,finished_at=now(), discovered_count=$4,indexed_count=$5,failed_count=$6,error_summary=$7 WHERE id=$1
