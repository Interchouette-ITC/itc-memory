UPDATE branches b SET is_current=EXISTS(SELECT 1 FROM repository_locations l WHERE l.repository_id=b.repository_id AND l.current_branch=b.name AND l.is_present) WHERE b.repository_id=$1
