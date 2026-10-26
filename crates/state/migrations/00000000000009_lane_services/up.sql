-- Services a plain `devlanes start` brings back: the ones the lane ran and nobody
-- stopped. Lanes from older daemons start with none and fall back to their profiles.
ALTER TABLE lanes ADD COLUMN services TEXT NOT NULL DEFAULT '[]'
    CHECK(json_valid(services) AND json_type(services) = 'array');
