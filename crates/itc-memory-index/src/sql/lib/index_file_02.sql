UPDATE source_documents SET last_seen_at=now(),is_current=true,freshness_status=$3 WHERE repository_location_id=$1 AND path=$2
