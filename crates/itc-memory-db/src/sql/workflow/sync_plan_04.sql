UPDATE phases SET status='removed',updated_at=now() WHERE plan_id=$1
