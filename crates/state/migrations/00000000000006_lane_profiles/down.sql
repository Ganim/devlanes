-- Preserve every selected profile for daemons using the old encoded format.
UPDATE lanes SET profile = CASE json_array_length(profiles)
    WHEN 0 THEN NULL
    WHEN 1 THEN json_extract(profiles, '$[0]')
    ELSE profiles
END;
ALTER TABLE lanes DROP COLUMN profiles;
