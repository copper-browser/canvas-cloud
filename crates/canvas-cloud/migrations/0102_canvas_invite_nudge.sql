-- Invite reminders and revocation (see docs/canvas.md).
--
-- `nudged_at`: when someone last re-sent a pending invite, which reminds the invitee (a
-- `canvas` event with kind `invited`). NULL until the first reminder. Reminders are throttled
-- per invite, measured from COALESCE(nudged_at, created_at).
--
-- Status `revoked`: the canvas owner or the original inviter withdrew a pending invite. Like
-- `declined`, it frees the (canvas, email) slot, so the same email can be invited again.
-- Additive only: existing rows and older clients are unaffected.

ALTER TABLE canvas_invites ADD COLUMN nudged_at timestamptz;

ALTER TABLE canvas_invites DROP CONSTRAINT canvas_invites_status_check;
ALTER TABLE canvas_invites ADD CONSTRAINT canvas_invites_status_check
    CHECK (status IN ('pending', 'accepted', 'declined', 'revoked'));
