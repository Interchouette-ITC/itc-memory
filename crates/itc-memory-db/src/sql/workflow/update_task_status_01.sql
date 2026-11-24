UPDATE tasks SET status=$2,status_source='agent',updated_at=now(),completed_at=CASE WHEN $2='done' THEN now() WHEN status='done' THEN NULL ELSE completed_at END WHERE id=$1 RETURNING phase_id
