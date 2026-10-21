ALTER TABLE lanes ADD COLUMN profiles TEXT NOT NULL DEFAULT '[]'
    CHECK(json_valid(profiles) AND json_type(profiles) = 'array');

-- Old daemons stored either a plain name or a JSON array in profile. Values that
-- are not arrays of strings remain literal names, matching the legacy decoder.
UPDATE lanes SET profiles = CASE
    WHEN profile IS NULL THEN '[]'
    WHEN json_valid(profile) THEN CASE
        WHEN json_type(profile) = 'array' THEN CASE
            WHEN NOT EXISTS (SELECT 1 FROM json_each(profile) WHERE type != 'text')
                THEN json(profile)
            ELSE json_array(profile)
        END
        ELSE json_array(profile)
    END
    ELSE json_array(profile)
END;
UPDATE lanes SET profile = json_extract(profiles, '$[0]');
