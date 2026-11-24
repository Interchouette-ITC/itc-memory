UPDATE tasks SET status='cancelled',status_source='plan',updated_at=now() WHERE id=$1
