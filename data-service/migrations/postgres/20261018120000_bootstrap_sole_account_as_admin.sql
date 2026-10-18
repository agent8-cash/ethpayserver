-- Promote the SOLE registered account to server_admin, once, where nothing is.
--
-- WHY THIS EXISTS. Nothing in the server ever writes `server_admin`: the role
-- can only be granted through the admin UI, which requires an existing admin to
-- reach. So a database with no admin in it has no path to one, and every admin
-- surface - plugin install, user management, server settings - is unreachable
-- for as long as that holds. Testnet entered that state twice in two days, each
-- time a wipe recreated the database; the documented way out was raw SQL against
-- the running server, which is not a thing to keep doing.
--
-- Measured on testnet 2026-10-18, with the single account's API key: 403
-- "Admin access required" on /api/admin/*, 200 on /api/stores, and 401 for a
-- garbage token - so the key authenticates and the account simply lacks the
-- role. Every admin gate in the server is a bare `role == Role::ServerAdmin`
-- comparison, so no key scope can substitute for the row being right.
--
-- THE GUARD IS THE WHOLE DESIGN, and it is deliberately narrower than the
-- problem:
--
--   exactly one account exists   -- so this can never pick a winner between two
--   AND no server_admin exists   -- so it can never alter a server someone
--                                   already administers, including by demoting
--                                   nobody and silently adding a second admin
--
-- Both conditions are evaluated in one statement, so there is no window between
-- reading them and acting. On an empty database (every fresh mainnet deploy)
-- the count is 0 and this is a no-op; on a server with two or more accounts it
-- is a no-op and the operator uses the admin UI. Re-running it after it has
-- promoted the account is also a no-op, because that account is then an admin.
--
-- THIS IS NOT THE FIX. A one-off promotion cannot stop the next fresh deploy
-- having no administrator; the mechanism for that is the first registered
-- account becoming server_admin at registration time, which carries a decision
-- about mainnet that is not this migration's to make. This exists so that the
-- server standing up today has an administrator without anyone typing UPDATE
-- into production.
UPDATE users
   SET role = 'server_admin'
 WHERE (SELECT COUNT(*) FROM users) = 1
   AND NOT EXISTS (SELECT 1 FROM users WHERE role = 'server_admin');
