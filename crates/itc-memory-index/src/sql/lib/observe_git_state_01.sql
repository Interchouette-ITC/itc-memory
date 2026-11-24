SELECT current_branch,head_sha FROM repository_locations WHERE machine_name=$1 AND root_path=$2
