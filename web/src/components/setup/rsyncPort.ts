/**
 * Validation for the optional rsync SSH port (`RSYNC_SSH_PORT`).
 *
 * Empty means the SSH default of 22, which is what almost every setup
 * wants. A typo still has to be caught here: the only later symptom is an
 * archive log full of "Connection refused", long after the wizard reported
 * success. The setup runner rejects the same values server-side.
 *
 * Its own module so the Archive step's inline field error and the wizard's
 * step-level gate share one rule, without a component module having to
 * export a non-component (which breaks Fast Refresh).
 */
export function rsyncSshPortError(raw: string | undefined): string | null {
  const value = raw?.trim() ?? ""
  if (!value) return null
  if (!/^\d+$/.test(value)) return "SSH Port must be a number."
  const port = Number(value)
  if (port < 1 || port > 65535) return "SSH Port must be between 1 and 65535."
  return null
}
