-- Workspace avatar: nullable column on organizations, mirroring the
-- 2048-octet cap on users.avatar_url (migration 14). NULL means "render
-- the default app-logo tile on the FE"; non-NULL is the public
-- assets-origin URL returned by /api/uploads/workspace-avatar.

ALTER TABLE organizations
    ADD COLUMN avatar_url TEXT
        CHECK (avatar_url IS NULL OR octet_length(avatar_url) <= 2048);
