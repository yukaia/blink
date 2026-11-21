# Changelog

Notable changes to blink. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases before 0.7.0 predate this file and are not reconstructed here; see
the git history for those.

## [Unreleased]

### Added

- **SFTP logs in by keyboard-interactive** when a server takes passwords
  only that way (PAM with `PasswordAuthentication no`), answering its single
  hidden password prompt with the session's password, as OpenSSH does. The
  password is given once; a second factor or any other question fails with
  what was asked.

- **More languages are highlighted**: Windows batch, PowerShell, INI-style
  config (`.ini`, `.conf`, `.cfg`, `.properties`), and the C family (C,
  C++, Go, Java, C#, Kotlin, Swift, CSS and others), each with its own
  comment and string rules. Files these cover, such as `.scala`, `.cmd`
  and `.properties`, now open in the viewer.
- **known_hosts reads hashed names and wildcard patterns**, so an
  OpenSSH `known_hosts` copied in works: `|1|salt|hash` names from
  `HashKnownHosts=yes` (Debian and Ubuntu's default), and patterns such
  as `*.example.com`. Before, they matched nothing, so every such host was
  asked about as new, and a man in the middle got the "accept?" prompt
  rather than a refusal. `known-hosts remove` takes out hashed names but
  never a pattern.
- **Ctrl+C quits**, from any screen, through the usual quit prompt; a
  second Ctrl+C on the prompt quits at once.
- **The log scrolls.** Focus it and use the arrows or page keys, or the
  wheel over it. Scrolled back, it holds its place as lines arrive and
  shows how many are newer.
- **Remote symlinks are marked ↪ and open like what they point at.** Enter
  or a double-click on a link to a folder opens the folder; a
  double-click on a link to a file downloads it. Over FTP, where the
  server can't say, a link is treated as a file.

- **Themes fall back to 256 colours** on a terminal that doesn't say it
  shows 24-bit colour (via `COLORTERM`, or by being one known to), such as
  macOS Terminal. Each colour becomes its nearest in the 256-colour
  palette, rather than whatever the terminal made of a 24-bit one. Set
  `COLORTERM=truecolor` if yours is wrongly left out.

### Changed

- **Queued transfers are listed in the Transfers pane**, dimmed and
  marked "queued", below the running ones, and `c` / `C` act on them.
  Only running jobs could be picked before, so with nothing running a
  queue couldn't be cancelled, and a queued job outside a batch never
  could. The pane's corner reads "+N queued, paused" while a pause holds
  the queue.
- **`t` over a session with its own theme saves it to that session.** It
  always saved to `config.ini`, which changed the theme everywhere else
  while the session's own came back on the next connect. A session
  without its own theme, and the session list, still save the default.
- **Long log lines wrap.** A message wider than the log pane ran off its
  edge, and for some errors the log is the only place the reason is
  given. Each entry now wraps under its timestamp, breaking at spaces;
  scrolling moves by rows, and "↓ N newer" still counts entries.
- **The status bar shows the keys for the focused pane.** It showed the
  file keys everywhere, `[space] select` over the Transfers pane
  included, and never the keys that act there. It also names `^u` upload
  beside `^d` download.
- **The FTPS toggle in the session editor says what it does.** It was
  labelled `tls verify:`, where a tick meant the chain was *not* checked,
  and ticked it warned that it "disables TLS protections". Since pinning,
  it pins the server's certificate instead: asked about on the first
  connect, the handshake still verified, later connects held to it. It now
  reads `tls trust: [x] pin the certificate, not the CA chain`.
- **`scp://` reads like `sftp://`.** It shares SFTP's colour in the
  session list, being the same encrypted connection, and the "routed
  through SFTP" note on connecting is information rather than a warning.

- **FTPS trusts the system's certificate store** as well as the built-in
  Mozilla bundle, so a server whose certificate comes from a CA installed
  on the machine (a company's own, say) verifies without pinning.
- **A pinned FTPS certificate no longer has to name the host.** With
  `accept_invalid_certs`, the certificate is the identity and the
  handshake signature proves the server holds its key, so the hostname
  check added nothing but refused the certificates pinning is for:
  self-signed ones with only a common name (rustls reads only the subject
  alternative names), and servers reached by an address their certificate
  doesn't list. They are now offered for trust like any other.
- **A server that never greets is reported within 15 s,** as such rather
  than as the whole connect timing out, with a hint that it may be an
  implicit FTPS port, which blink doesn't speak. A failed connect to port
  990 says the same.

- **Anonymous FTP and FTPS don't ask for a password.** A session with no
  username logs in as `anonymous`, which ignored whatever password was
  typed into the prompt.
- **A transfer whose connection drops is retried once**, on a fresh
  connection, whichever connection it was using; a download resumes from
  its partial file. Only connections reused between jobs got the retry.
- **Idle transfer connections close after a minute**, and when
  `parallel_downloads` is lowered. They stayed open until disconnect,
  counting against servers' per-user connection limits.
- **`t` changes only the `theme` line of `config.ini`.** Saving rewrote
  the whole file from the settings blink had loaded, so every `t` dropped
  your comments and blank lines, undid anything you'd changed in the file
  since blink started, and replaced a file it couldn't parse with the
  defaults. The first run now writes a commented `config.ini`.
- **An unusable `theme` name falls back to dracula** with a warning, as a
  missing theme does, instead of stopping blink.
- **Only configuration problems are called "config error".** An
  unreadable `known_hosts`, a checkpoint problem or a missing `$HOME`
  now say what went wrong without that label, and a bad URL to `blink
  connect` reads `invalid URL: …`. A problem in `config.ini` names the
  file.
- **Plain text isn't highlighted.** Text, Markdown, logs and `.nfo` files
  were coloured as code: an apostrophe opened a "string", a URL's `//` or
  an issue's `#12` a comment, and `/*` in a path turned the rest of the
  file into one.
- **Only the TUI reads `config.ini`.** A broken one still stops the TUI,
  but no longer stops `blink sessions`, `themes`, `checkpoints` or
  `known-hosts remove`, which don't use it.
- **Listing creates nothing.** `blink sessions`, `themes` and
  `checkpoints` left a `config.ini` and every folder behind on a fresh
  machine; the TUI creates them when it starts.
- **Large images are scaled to fit instead of refused.** The viewer
  turned away anything over 4096 pixels a side, which ruled out most
  phone photos and every panorama. Now any image that can be decoded in
  512 MiB (about 170 megapixels; it was 128 MiB) is shown, scaled down to
  the viewer. Photos are scaled before they're turned the right way up,
  so a large one isn't copied at full size just to be rotated.
- **`blink themes` lists your own themes too**, marked `(user)`, since `t`
  cycles through them. A theme file that won't load, or whose name can't be
  used, is a warning on stderr, as with `blink sessions`.
- **Theme names are matched one way everywhere**: ignoring case, with
  spaces as hyphens. Built-ins already were; user theme files and the `t`
  cycle weren't. On Linux this means a user `Dracula.ini` now replaces the
  built-in `dracula` instead of being a separate theme.
- **A user theme can leave colours out**; they come from `dracula`.
  Previously every colour was required, so a new one would have broken
  every existing theme file.
- **tokyo-night shows errors in their own red.** They were the accent's
  pink, so an error line looked like any other highlight.
- **solarized-dark uses Solarized's own text colours**: base0 for text and
  base01 for dimmed text. It used base2, the light variant's background,
  which was much brighter than the scheme intends.
- **On Windows, the config folder is now `%APPDATA%\blink`.** It was in
  `Documents\blink`, a folder meant for your own files, and one that
  OneDrive often syncs, taking your `known_hosts` and sessions with it.
  Existing files are not moved: move `Documents\blink` to `%APPDATA%`
  yourself to keep them.

### Security

- **A cancelled connect can't take over the next one.** Esc while
  connecting left the connect running, and its result had nothing to say
  which connect it answered. Cancel a slow connect to one server, pick
  another session, and the first server's late connection was taken as the
  second's: the screen and the transfer workers said one server while the
  file pane browsed, renamed and deleted on the other. Its failure ended
  the new connect, blaming it. A cancelled connect is now stopped, and a
  result from any but the current attempt is dropped.
- **A password or passphrase can't outgrow its buffer.** The buffers are
  sized so typing never copies the secret into a new allocation, leaving
  the old one unwiped, but a 257th character, typed or pasted, did just
  that. Keys past the buffer's size are now refused.
- **A host-key mismatch during connect drops the password typed for it.**
  The failed connect that follows the warning does nothing, so the warning
  can stay up, and with it nothing cleared the password and passphrase
  state: the password for a server that had just failed to prove its
  identity stayed in memory until the next connect or quit.
- **Session names and hosts are drawn sanitized everywhere.** Only the
  save offer cleaned them; the selector, header, prompts, editor and the
  host-key dialog printed them raw, where a right-to-left override in a
  hand-written session file or a typed URL vanished, so the host shown
  above "verify this fingerprint" could read as another one. They are now
  replaced with a space, as in remote names.
- **A download checks for planted symlinks as it starts, not only when it
  was planned.** A recursive download refused a folder that was another
  user's symlink, but only while walking the tree; the downloads ran after
  the overwrite prompt, which can stay open indefinitely, and a symlink
  planted in between was followed. Each download now checks every folder
  above its file first. Symlinks root owns are trusted, as only root can
  make them and some systems' own paths are them (macOS's `/tmp`).

- **A download's empty folders get the same planted-symlink check.** The
  folders of a tree with no file in them were made directly, following a
  symlink another user had planted under a folder's name. A download into
  a shared folder could then create folders in the link's target, such as
  making `~/.ssh/authorized_keys` a folder. They are now refused and
  reported, as a file's download is.

- **An SFTP upload no longer writes through a symlink planted as its
  partial.** The partial's name is predictable and a server's open follows
  links, so in a shared remote folder someone could aim one at any file
  your account can write, and the upload overwrote it. The name is now
  cleared and the partial created exclusively, which fails rather than
  follows a link that can't be removed.

- **A download no longer resumes onto a partial someone else planted.** In
  a folder other users can write to, such as `/tmp`, the partial's name is
  predictable, and a sidecar naming a public file's server, path, size and
  modification time was enough for blink to append the real file's tail to
  another user's bytes and rename the result into place, still theirs to
  rewrite. On Unix the partial and its sidecar must now be your own files
  with a single name, and the partial is checked again just before the
  rename. The sidecar is opened without blocking, so a FIFO planted in its
  place can't hang the download, and only 64 KiB of it is read.
- **A remote entry whose name contains `/` is no longer acted on.** A
  server could list `sub/file`, or `/etc/x`, and a delete, rename or view
  of the entry shown went to another path. Such names are skipped, as `.`
  and `..` already were.

- **An FTPS first connect refuses a certificate it failed to record.**
  When the session had no pin, unreadable verifier state read as "no
  certificate to ask about", and the connection would have carried on
  with no prompt and no pin. Nothing reached that path in practice.

- **On FTP, a folder blink may not list is no longer taken for empty.**
  Every `550` reply read as "no such file", including `550 Permission
  denied`, so the upload overwrite check skipped the prompt and files in
  such a folder were replaced without asking. A `550` is now read by what
  the server says; one that says neither is still taken as not found.
- **A remote file name can't forge a line in the debug log.** A newline
  in a name logged through a `%` field started a new line, so a server
  could plant a convincing fake entry in `BLINK_LOG_FILE`. It's now
  written as `\n`.
- **More invisible characters are shown as spaces**: the combining
  grapheme joiner, the Khmer inherent vowels, the line and paragraph
  separators, the Braille blank (the usual "invisible name" trick) and
  the shorthand format controls. A variation selector is kept only where
  it changes how something is drawn (an emoji, a keycap, a symbol or
  ideograph variant): after a plain letter it draws as nothing, so `a`
  with one looked exactly like `a`.
- **"Accept and save" no longer trusts two keys for one host.** If
  another blink saved a different key for the host while the prompt was
  open, both were kept and both trusted. The connection is now refused,
  and the log says why.
- **A saved host key can't be left half-written.** The entry was written
  in pieces and not synced, so a crash could leave a key cut short, and
  that host then read as a changed key.
- **`blink themes` can't print a raw escape sequence.** A theme file
  named with one was listed as is, so its name could drive the terminal.
  Theme names with control characters are now refused, and the listing is
  sanitized as well.
- **Files blink writes are private on their own** (mode 0600 on Unix).
  They took the umask's default, typically 0644, so they were private only
  while their folder was, and a `chmod 600` lasted until the next save.
- **A config folder open to other users is made private.** One made by
  hand or restored from a backup at 0755 stayed that way, leaving every
  session and `known_hosts` readable by other users. It's now tightened
  to 0700 and the log says so.
- **More invisible characters show up in file names.** blink already shows
  bidi overrides and zero-width spaces in server-supplied names as spaces,
  so two names can't look identical. Soft hyphens, word joiners, invisible
  operators, Unicode tag characters and the Hangul fillers slipped past
  that and now get the same treatment.
- **Deleting a folder won't follow a symlink that replaced it.** A remote
  folder you chose to delete, replaced by a symlink before you confirmed,
  was emptied through the link: whatever it pointed at lost its files.
  blink now checks the folder is still a real directory before deleting
  it, and on SFTP every subfolder too, and stops with an error if not.
- **A host key that can't be encoded is refused.** Such a key was checked
  and stored as an empty one, which any other key failing the same way
  would have matched. No known key fails to encode; the connection is now
  rejected if one does.
- **`known_hosts` entries can't be written malformed.** Saving a host key
  refused a space in the host name but not a tab or a comma, either of
  which made a line that read back as a different entry.
- **Folder downloads don't follow a symlink another user planted.** The
  fix above for partial files didn't cover the folders a recursive
  download creates: in a download directory other users can write to, a
  symlink planted under a folder's name sent every file below it wherever
  it pointed. blink now refuses to download into a symlinked folder that
  another user owns. Your own symlinks still work.
- **The debug log can't carry terminal escapes.** With `BLINK_LOG_FILE`
  set, remote file names and server messages were written to the log as
  they came, so one holding an escape sequence would run in the terminal of
  whoever viewed the log. Control and bidi characters are now written as
  spaces there, as everywhere else.
- **Downloads don't write through a symlink planted under the partial's
  name.** A `.blink-part` file and its `.meta` sidecar have predictable
  names, so anyone who could write to the download directory could plant
  symlinks there and have blink truncate, append to or write JSON into
  the file they pointed at. Partials are now created exclusively, resumed
  with `O_NOFOLLOW` on Unix, and a symlink under either name is discarded
  rather than followed. Only shared download directories (`/tmp`, a drop
  folder) were exposed.
- **FTPS asks before trusting a certificate on first use.** With
  `accept_invalid_certs` on, the first connect pinned whatever certificate
  it was shown, logging only that it had, so a man in the middle on that
  connect was trusted for good. blink now shows the certificate's SHA-256,
  in `openssl x509 -fingerprint -sha256` form, in the host-key prompt and
  waits for an answer before sending the password: accept and save pins
  it in the session, trust once pins it for this connection only. During
  that first connect, data connections must present the same certificate;
  before, they were accepted whatever they presented.

### Fixed

- **Closing the viewer stops a remote file still loading.** The fetch went
  on to the end, holding the connection every remote action waits on: Esc
  on a slow 25 MB image left browsing, refresh, rename and delete stuck
  until it arrived, and a disconnect meanwhile kept the connection open.

- **A remote text file stops loading at the text limit (1 MB).** It was
  fetched up to the image limit, 25 MB, and only then refused.

- **Viewing a remote symlink asks what it points at first**, as Enter and
  downloads do. A link to a folder is refused with that reason, not a bare
  read error, and one to a file too large to view is refused before its
  bytes are fetched.

- **Viewing a remote file with no connection says "not connected"**
  instead of flashing the viewer open and shut.

- **A local folder that couldn't be made is reported in the log.** Only
  the debug log heard of it, so an empty folder in a download could go
  missing without a word.

- **Skipping every conflict says there's nothing left to transfer.**
  Answering `s` when every file already existed did nothing, silently.

- **An upload name that can't be used on the server is reported**, as a
  download's is; it was dropped without a word. Entries skipped for a name
  that isn't valid UTF-8 are counted as "entries", since folders are too.

- **The local pane says when it couldn't read some entries**, once per
  change, as the remote pane does. Entries `read_dir` or their metadata
  couldn't read, on a flaky network mount say, were left out without a
  word.
- **F5 on the local pane no longer logs "refreshed"** before the listing
  has run, and whether or not it then worked; the remote pane never did.
- **A double-click acts on the entry it landed on.** The two presses were
  matched by row, so a refresh between them, as the remote pane does when
  each upload lands, could put another entry at that row and open or
  transfer that one instead.
- **The wheel over a file pane focuses it**, as over the transfer list. It
  moved the cursor of an unfocused pane, which isn't drawn there, and
  Ctrl+D or Ctrl+U then transferred the entry it had moved to, unseen.
- **The offers after connecting wait before taking a key**, as the other
  prompts that open on their own do. A key typed as the connection came
  up answered the resume offer, and `d` there deletes the batch's partial
  files; a held or doubled key answered the next offer too.
- **The filter is dropped on moving to another folder.** It filtered the
  next folder's listing as well: entering `photos-2024` under a `photo`
  filter showed it near empty.
- **Esc at the password prompt logs "connect cancelled"**, as the
  passphrase prompt and the connecting screen do.
- **Cancelled transfers aren't logged as failures.** Each cancelled job
  logged a red `failed: …: cancelled` line, after the cancel had already
  said so: a batch cancel buried its own summary under them and read as a
  run of failures. A cancel is now its own event, so an error that merely
  reads "cancelled" also counts as the failure it is.
- **Queuing a batch no longer logs a line per job**, which pushed the
  "queued downloads: N file(s)" summary and the queue-full notice out of
  the log as soon as they were written.
- **An empty folder uploaded into the remote pane shows up** without F5.
  The pane refreshed when a file landed, never for a folder being made.
- **Replies to a rename, new folder or delete from a closed connection
  are dropped**, as a listing's are, rather than logged and refreshed
  against the server connected since.
- **A batch cancel counts the jobs waiting for room in the queue.** Past
  the 100,000-job queue limit, jobs wait outside it; confirming cancelled
  them, but the question and the log line left them out.
- **`r` retries the failures of a batch another blink kept from being
  saved.** With another blink holding the session's checkpoint, a batch
  runs unsaved, its failures held in memory only. `r` threw that record
  away and went to the file: while the other blink held it, the failures
  were lost; once it let go, `r` resumed the other blink's batch instead.
  They are now retried from memory.
- **Files waiting for room in the queue aren't queued twice.** Past the
  100,000-job queue limit, jobs wait outside the queue, and queueing the
  same files again didn't see them: each went into the checkpoint and the
  queue a second time, and ran twice once the first copy finished.
- **`r` without a connection says "not connected"** rather than that no
  checkpoint was found.
- **Enter on an unchanged name in the rename box renames nothing.** The
  box was filled with the name as the pane shows it, cut in the middle
  past 512 characters, and trimmed before the check: F2 then Enter renamed
  a long name to its shortened form, `…` included, and `notes ` to
  `notes`. It now offers the whole name and leaves it alone unless you
  change it. A name shown with control characters replaced still renames
  on Enter, which is how such a file is cleaned up.
- **Saving a session can't replace a session file that didn't load.**
  Renaming in the editor or saving with Ctrl+S checked only the sessions
  in the list, which leaves out a file that fails to parse, so a save
  under its name overwrote it, hand edits and all. Both now refuse and
  say the file can't be read.
- **A delete confirmed after the connection dropped says it didn't
  happen**, as rename and new-folder do, rather than nothing at all.
- **Queued transfers show in the Transfers pane.** Only running jobs had
  rows, so a batch waiting behind the parallel limit or a pause was
  invisible: paused before anything started, the pane said "no active
  transfers". The pane's corner now reads "+N queued", and with nothing
  running it says how many wait.
- **A long file name keeps its size and extension on screen.** The name
  was never shortened, so it pushed the size off the pane; it now loses
  its middle, as a transfer's does.
- **Wide characters no longer push text off the panes.** Shortened paths,
  the filter and transfer names were measured in characters, so CJK or
  emoji names came out wider than their room: a path lost the tail that
  says where the pane is, and a transfer row lost its speed.
- **The file pane footer says "1 item"**, and "3 of 400 items" while a
  filter hides some.
- **The viewer's line number matches the text after a resize.** Making
  the terminal taller at the end of a file showed an earlier page while
  the hint still named the old top line.
- **Help says `r` and `R` work in the Transfers pane**, as it did for `c`
  and `C`; pressed in a file pane they do nothing.
- **The selector says "(unchanged)", not "(home)", for a session without a
  local folder**, since connecting leaves the local pane where it is.
- **Session names with wide characters keep the list's columns lined up.**
  The name was padded by characters, not terminal columns.
- **A refresh keeps your selection and your place.** The remote pane
  refreshes by itself as each upload into it lands, and that cleared every
  selection and left the cursor at its old position, on another file once
  new ones sorted in above. A download pressed next fell back to the
  cursor's file and fetched that one alone. A refresh of the folder shown
  now keeps the selections still listed and the cursor on the same file; a
  listing of another folder starts fresh, as before.
- **The filter finds long names by any part.** It searched the displayed
  name, which has its middle cut out when very long, so a query in the cut
  part found nothing.

- **Names Windows can't store are skipped up front.** A remote file whose
  name has `< > " | ? *` or a control character was planned and then failed
  as a job of its own; on Windows it is now counted with the other names
  that can't be used locally.
- **A broken symlink at a download's destination asks first.** The conflict
  check followed links, so one pointing nowhere read as nothing there, and
  the download replaced it without asking.
- **An upload walk counts local entries it couldn't read.** One whose type
  couldn't be read was skipped without a word; it is now counted and
  reported like the rest.

- **A failed start no longer leaves the terminal in raw mode.** Raw mode
  went on first, and if switching to the alternate screen failed after it,
  blink exited with it still on: a shell with no echo and no line editing.
- **Leaving the TUI undoes everything it can.** A failure to turn raw mode
  off stopped the rest, leaving the alternate screen up and the cursor
  hidden; and a failure to restore the terminal was dropped silently. Each
  step now runs regardless, and a failure is reported.
- **A background task's crash is shown in the log.** It went only to the
  debug log, which goes nowhere unless `BLINK_LOG_FILE` is set, and the
  task never sent the answer the screen was waiting for, so a connect or a
  listing could wait forever with no reason given.

- **The bottom pane's tabs answer only where they're drawn.** In a terminal
  too narrow for the title, the labels are cut short, but their click areas
  ran on past them, so a click on the pane's corner switched to a tab
  nobody could see.

- **A stall no longer sets off a burst of redraws.** The redraw tick made up
  every tick it missed, so a one-second stall (a large image rendering)
  was followed by ten full redraws back to back. Missed ticks are now
  skipped.

- **A plain PKCS#8 key loads even with a passphrase given.** russh decodes
  it as an encrypted one when handed a passphrase, and failed on its
  structure. A passphrase left over for a key that no longer needs one (the
  file replaced since it was asked for) no longer stops it loading.

- **A failed SFTP login says why.** It read "rejected by server" whatever
  happened; it now names the methods the server would take, or says it
  accepted the credential but wants another step as well.
- **SFTP uploads check what the server stored**, as FTP's do, before the
  rename: a server that acknowledged writes it dropped had its short file
  put in place as complete.
- **SFTP folder operations wait as long as transfers do.** Listing, stat,
  rename, delete and preview had russh-sftp's 10 s limit per request, so a
  slow server read as a dropped connection; they now get the same 60 s as
  transfers and FTP.
- An SFTP connect error puts an IPv6 address in brackets
  (`[::1]:22`), and the host-key timeout log line names the host as
  known_hosts does.

- **A server without SFTP says so, at once.** blink sent the request for
  the `sftp` subsystem without waiting for the answer, so a refusal showed
  up only as a failed SFTP start, after a wait, with no reason given. It
  now fails immediately with "the server refused SFTP"; for an `scp://`
  session, it adds that blink's SCP runs over SFTP and doesn't speak the
  legacy protocol such a server may offer.

- **A stale partial that can't be removed is named.** In a sticky folder
  such as `/tmp`, a partial another user left there can't be deleted, and
  the download failed with a bare "File exists". It now says which file is
  in the way.
- **A partial's sidecar is flushed when written.** It went through a tokio
  file that was dropped without a flush, so the write could still be in
  flight, with any error lost.

- **FTP shows hidden files, and deletes them with their folder.** blink
  now asks for `LIST -a` (falling back to a plain `LIST` on servers without
  it): servers such as vsftpd left dotfiles out, so they didn't show, and
  deleting a folder holding one removed everything else, then failed.
- **An FTP download whose `SIZE` or `MDTM` goes unanswered is retried**
  on a fresh connection instead of carrying on out of step with the
  server, which could throw away a partial download that should have
  resumed.
- **A dropped FTP data connection is retried** like any other dropped
  connection, rather than failing the transfer.
- **FTP uploads are checked against what the server stored.** Where the
  server answers `SIZE`, a short upload is an error instead of being put
  in place.
- **Creating an FTP folder where a symlink to a folder already is
  succeeds**, as on SFTP.
- **A failed FTP download no longer leaves an empty folder behind.**
- **FTP and FTPS work with IPv6 servers.** Data connections used PASV,
  which can't describe an IPv6 address and which IPv6 servers commonly
  refuse, so every listing and transfer failed after a good login. Over
  IPv6 they now use `EPSV`.
- **A busy FTP server isn't reported as a wrong password.** Every login
  failure read `authentication failed`, including `421 Too many
  connections` and a reset mid-login; only a refused login (`530`) does
  now, and the rest are treated like any other dropped connection.
- **FTP connect errors bracket an IPv6 address** (`[::1]:21`).
- **Transfer errors keep the server's reason.** A refused FTP transfer
  showed `not found: stor /path` whatever the server said (permission
  denied, disk quota exceeded); SFTP errors dropped the server's message
  too.
- **A lost FTP data connection is retried** (`425`, `426`), and an SFTP
  channel that's out of step with the server is replaced rather than
  used on.
- **Quitting no longer hangs on an FTP server that went away.** Closing
  each idle transfer connection waited for a reply to `QUIT`, which from a
  server that vanished silently took until TCP gave up (about 15 minutes).
  Closes now give up after 3 seconds, and a failed transfer is marked
  failed without waiting on its connection.
- **A resumed download's speed starts from where it resumed**, not from 0,
  which showed a speed of many GB/s for its first moment.
- **A stalled transfer's speed falls to 0** instead of showing its last
  speed for as long as it stalls.
- **A host-key prompt whose transfer was cancelled closes itself.** It
  stayed up, and answering it did nothing.
- **Two blinks on one session no longer wipe each other's batches.** Each
  wrote its own copy of the checkpoint, and the last to save won. The
  first now holds a lock; the second runs its batches unsaved, says so,
  and leaves that checkpoint alone (no offer, resume or discard), as does
  `blink checkpoints`.
- **Clearing a checkpoint keeps partial downloads that aren't its own.**
  Partials were removed by path alone, so a newer batch downloading into
  the same place lost its partials when the old checkpoint was cleaned
  up. A partial whose sidecar names another file or server is now kept.
- **Discarding an upload checkpoint removes its partial files on the
  server** while connected; they were left there for good.
- **A checkpoint with a malformed `version`** (a string, `null`) is
  refused instead of being read as version 1, which reset every finished
  job to pending.
- **A checkpoint file is checked against its name and contents**: its
  session and direction must match the name, every job must belong to
  its direction, and local paths must be absolute.
- **`blink checkpoints` prints paths safely and lines up its columns.**
- **A symlinked `config.ini` stays a symlink** when blink saves; it was
  replaced with a plain file, cut off from your dotfiles.
- **A `config.ini` with a byte-order mark is read.** Every setting in it was
  silently ignored.
- **`config.ini` says what it ignored**: unknown settings and sections,
  keys in the wrong case, a setting given twice, and a clamped
  `parallel_downloads` are reported in the log. A second `[general]`
  section is read rather than skipped.
- **Config errors name the setting and the value**: `mouse must be true or
  false …, not \`sometimes\``.
- **Long messages keep their end.** Every log line and error was cut to
  its first 512 characters, so a long path pushed out the reason
  (`…: permission denied`). They now lose their middle instead.
- **Tab stops in the viewer count wide characters as two columns**, so a
  line with CJK text before a tab lines up.
- **Highlighting follows strings across lines**: Rust strings, shell and
  YAML quotes, JavaScript template literals, Python and TOML triple
  quotes, and any string continued with a backslash. Their second line
  was coloured as code, and the closing quote opened a new string.
- **Highlighting fixes per language**: `#` in the middle of a shell word
  (`foo#bar`, a URL's `#frag`) or a YAML value isn't a comment; shell's
  `$#`, `$?` and friends are variables; JavaScript template literals are
  strings; Rust raw strings (`r#"…"#`) are one string, and nested block
  comments nest; TOML array elements on their own line aren't section
  headers, and a header's trailing comment is a comment; YAML keys can
  hold `-`, `.`, `/` or quotes, and single-quoted values are strings,
  while an apostrophe inside a plain value isn't; numbers stop at the
  word they start (`3files`, `1each`).
- **File names map to the right language**: `.bat`/`.cmd` as batch, not
  shell; `.ps1` as PowerShell; `prod.env`, `.env.local`, `Dockerfile.dev`
  and `Containerfile` as shell.
- **An unreadable known_hosts says why it refused the connection.** Only
  the debug log knew, so every SSH connect just failed. One non-UTF-8 byte
  (a Latin-1 comment) was enough; such a file is now read anyway.
- **A host key that couldn't be saved is reported, and not asked about
  again** by each transfer worker mid-transfer: it's trusted for the
  session, as you chose.
- **`known-hosts remove` can't lose a key being saved at the same time.**
  Both now hold `known_hosts.lock`; the lock on `known_hosts` itself
  guarded nothing once the file was replaced.
- **`known-hosts remove` keeps CRLF line endings.**
- **Hosts the file can't hold are refused when saving**: a name starting
  with `#`, `!`, `@` or `|`, or holding `*` or `?`, read back as something
  else and was asked about on every connect.
- **Errors read as sentences**: `error: session not found: x`, not
  `Error: SessionNotFound("x")`.
- **Logging problems are reported where you'll see them.** An unusable
  `BLINK_LOG_FILE`, or one other users can read, was warned about just
  before the TUI took the screen; it's now in the log. An invalid
  `BLINK_LOG` was ignored without a word; it's now warned about too.
- **`blink open` names session files it couldn't read** when nothing
  matches, instead of only saying "session not found".
- **`blink sessions` brackets IPv6 hosts** (`u@[::1]:22`) and lines up
  long and wide-character names.
- **`blink themes` warns when the configured theme won't load**, since
  the TUI would fall back from it.
- **Notices aren't lost when a command fails** before the TUI starts.
- **A save no longer reports failure after it succeeded.** On filesystems
  that won't sync a folder (some FUSE and network mounts), every save said
  it failed though the file had been written.
- **On the BSDs, a missing config location is an error**, as it is
  elsewhere, instead of putting the config in `blink/` under whatever
  folder blink was started from. They now use the same XDG rules as Linux.
- **A file where a config folder belongs is reported as such**, not as a
  "Not a directory" error from whatever tried to open something inside it.
- **A config folder made private mid-session is reported at once.** The
  first `t` creating `themes/`, say, was only reported at the next launch.
- **Control characters in a session name become `_` in its file name on
  every platform**, not only Windows; that includes DEL and the C1 range.
  A session or checkpoint whose name holds one is saved under a new file
  name; the old file is not moved.
- **Leftover temp files from a crash are cleaned up** at the next start,
  once over an hour old. Before, only `blink checkpoints --force` removed
  any, and only in the checkpoint folder; it now removes only files named
  as blink's own temp files.
- **One broken theme file no longer stops `t`.** It was retried on every
  press, so no theme after it could be reached. It's now reported and
  skipped.
- **A theme that falls back at startup says why**, in the log. It always
  said "not found", even for a file with a bad colour, and said it on
  stderr just before the screen cleared, where it couldn't be read.
- **`t` moves on from the theme on screen**, a session's own theme
  included, and from a loosely spelled one like `Tokyo Night`. It started
  from the config's theme, or from the first theme.
- **Theme files with unusable names aren't offered.** A file named with
  `:` or `..` was listed by `t` but could never load.
- **Theme colours must be plain hex.** `+1+2+3` and `###1a2b3c` were
  accepted as colours.
- **A theme name in wide characters fits the status bar.** It was measured
  in characters rather than columns, and could push the version off the
  edge.
- **Photos are shown the right way up.** Phone photos store their pixels
  unrotated and record the rotation in EXIF, which the viewer ignored, so
  portrait shots came out sideways.
- **Image previews work out of the box in WezTerm and foot.** WezTerm was
  sent kitty graphics, which it only accepts when configured to; it now
  gets iTerm2 images. foot wasn't recognised at all.
- **No blank image viewer inside tmux or screen, or in xterm.** Auto mode
  guessed a protocol from the outer terminal's variables, which tmux and
  screen don't pass images through, and guessed sixel for every xterm,
  though only one started with `-ti vt340` draws it. It now picks none
  there and says why; set `image_preview` to use one anyway.
- **Small images are no longer stretched in kitty and iTerm2.** Both were
  told a box of whole cells to fill, so a 12×8 icon became a 20×20 square.
  Where the terminal reports its cell size, images are now shown at their
  own size.
- **Sixel images stay inside the viewer.** Where the terminal doesn't
  report its cell size, as on Windows, a guess larger than the real cells
  drew images past the viewer's edge; the guess now errs small. And the
  last six-pixel band of a sixel image no longer runs past the panel.
- **Previewing a large image takes far less memory.** Scaling one down
  could briefly take 256 MiB on top of the decode limit.
- **UTF-8 `.nfo` files read correctly.** Every `.nfo` was decoded as DOS
  CP437, which mangled the accented letters in the UTF-8 XML that Kodi and
  other media managers write.
- **Every file the highlighter knows opens in the viewer**, including
  `.bashrc`, `.zshrc`, `.profile`, `justfile`, `GNUmakefile`, `.pyi`,
  `.pyw` and `.jsonc`.
- **Starting a batch no longer wipes out one waiting to be resumed.**
  After pressing esc at the resume offer, or after restarting blink, the
  first new batch in that direction replaced the interrupted batch's
  checkpoint. That batch could no longer be resumed, and its partial
  downloads were left behind with nothing pointing to them.
- **Renaming a session takes its checkpoints along.** Saving a connected
  session under a new name with ctrl+s, or renaming it in the edit form,
  left its checkpoints under the old name. `r` then found nothing to
  resume, and `blink checkpoints --clean` could delete them as orphaned.
- **Cancelling one transfer with `c` is final**, as cancelling a batch
  with `C` already was. The cancelled file used to come back in the next
  resume offer and on `r`.
- **Saving with ctrl+s no longer stores a certificate you trusted for
  one connection only.**
- **`blink checkpoints --clean` keeps the checkpoints of ad-hoc
  connections.** It deleted them, with their partial downloads, as
  orphaned, though connecting with the same `blink connect` URL would
  have offered to resume them. `--force` still removes them.
- **`blink checkpoints --force` removes every checkpoint file**, including
  ones it can't read and the temporary files an interrupted write leaves.
- **Listing checkpoints no longer rewrites old ones.** A plain `blink
  checkpoints` upgraded any version-1 file it listed.
- **`blink connect` refuses port 0, and an empty port means the
  default**, as in a session file. Port 0 connected nowhere, and the offer
  to save the session then failed.
- **Help no longer traps you in a prompt.** Opening help over the cancel
  prompt, then closing it, left that prompt answering every key by
  reopening itself. Help over the quit prompt did the same, leaving quit
  the only way out. Help now always returns where it was opened, and isn't
  offered on the quit prompt. Help opened from the session list also
  shows the session list behind it, not the file view.
- **`s` on a rename's overwrite prompt no longer replaces the file.** The
  prompt only offers `y` and `n`; `s`, which skips conflicts in a
  transfer's prompt, went ahead with the rename.
- **Ctrl and Alt combinations no longer type letters.** Ctrl+W in a text
  field typed `w`, and in the file view the letter shortcuts fired with
  Ctrl or Alt held: Ctrl+C on the transfers pane opened the cancel prompt.
  AltGr characters still type.
- **`?` can be typed into a new folder's name.** It opened help instead.
- **The transfer list scrolls.** Up to ten transfers run at once and the
  pane shows eight; the cursor, and what `c` would cancel, could be on one
  out of sight.
- **Uploads show ↑ in the transfer list.** Every row showed ↓.
- **A session's own theme applies.** `[appearance] theme` in a session file
  was saved and documented but never used. It now applies while that
  session is connected.
- **Ctrl+S won't overwrite another saved session.** Saving under a name
  another session already has is refused, as the edit form refuses it.
- **A changed-host-key warning waits until you're free.** Raised by a
  transfer mid-session, it took over at once: a form in progress was lost,
  and a `q` or Enter being typed dismissed the warning unread, which
  disconnects. It now opens once you're back at the file list, ignoring
  keys briefly as it opens, as the other prompts that arrive on their own
  do. Disconnecting also closes the viewer, so an image can't be redrawn
  over the session list.
- **The viewer's End and `G` show the last full page**, not the last line
  alone at the top.
- **The file panes agree with each other:**
  - F5 on the local pane keeps its rows and cursor while it lists, as the
    remote pane does, instead of blanking and jumping to the top.
  - The local pane shows no `..` at `/`.
  - The pane footer doesn't count `..` as an item.
  - The filter ignores case for every letter, not just ASCII ones.
  - The path line shows server names the way the file list does, and a
    long path loses its middle rather than its end.
- **A rename onto a name the filter hides asks to overwrite**, instead of
  failing on the server.
- **The overwrite prompt tells same-named files apart**, showing each below
  the folder they share: `a/readme.md` and `b/readme.md`, not `readme.md`
  twice.
- **Smaller fixes:** help lists `r`/`R`, `D`, Ctrl+C and the viewer and
  session-list keys, and says `p` pauses all transfers, not just
  downloads; the missing-graphics message lists sixel; a failed rename is
  logged by name, as a successful one is; and a selection with no usable
  names no longer logs "preparing 0 download(s)".

- **An FTP server that won't resume no longer blocks a download.** After
  an interrupted download, a server that refuses to resume (REST) failed
  every retry, and the file could only be downloaded after deleting its
  `.blink-part` by hand. The download now starts over instead.
- **A plain FTP rename no longer replaces a file.** Renaming onto a name
  that wasn't in the listing, but appeared since, replaced that file on
  servers that allow it. It now fails, as it does over SFTP; renaming over
  a file you confirmed still replaces it.
- **On macOS, two names written differently are no longer downloaded onto
  one file.** A server can hold `café` spelled two ways (composed and
  decomposed), which macOS treats as one name, so one download silently
  replaced the other. They now count as one destination, as `README` and
  `readme` already did.
- **Downloading a symlink to a folder downloads the folder** over SFTP and
  SCP, rather than failing as a file the server can't send.
- **Declining a download no longer leaves empty folders behind.** The
  download's folder tree was created while it was planned, before the
  overwrite prompt; it is now created when the download goes ahead.
- **A symlink whose target is gone can be renamed over a file** over SFTP.
  It was reported as not found.
- **A transfer that hits an internal error fails instead of hanging.** It
  stayed "running" for good, and resuming was refused for the rest of the
  session.
- **Dialogs fit small terminals.** Each dialog was a fixed share of the
  terminal's height, whatever it held, so on 80x24 nearly every one cut
  off its last lines: the keys to answer it, and any error message. The
  host-key mismatch warning lost its recovery command. Dialogs are now as
  tall as what they show, and wide enough for their longest line. Help,
  which is taller than 24 rows, scrolls with the arrow keys or the mouse
  wheel.
- **A session file with a bad port is refused.** A `port` that wasn't a
  number, such as `2222x`, fell back to the protocol's default without a
  word, so blink connected, and asked for the password, on another port
  than the file named. Such a file is now skipped with the reason, as is
  one with port 0. A missing or empty `port` is still the default.
- **Replacing a file on upload can't lose the old one.** On a server whose
  rename won't replace a file (FTP servers that refuse it, SFTP servers
  without `posix-rename`), blink deleted the old file and renamed the
  upload into place. If that rename failed too, the old file was gone and
  the new one sat under its `.blink-part` name. The old file is now moved
  aside to `<name>.<hash>.blink-old`, removed once the rename lands, and
  moved back if it doesn't. A folder in the way is no longer touched.
- **A symlink to a folder is a folder in the local pane.** It was listed
  as a file a few bytes long, so it could not be entered, and uploading
  it failed. A link to a file now shows the file's size.
- **Viewing a FIFO or device says so.** `v` on a local FIFO left the
  viewer on "loading" for as long as nothing wrote to it. Only regular
  files are opened now.
- **A new or renamed remote entry can't hold invisible characters.** The
  rename and new-folder prompts refused control characters but let
  through bidi overrides and zero-width characters, which a listing then
  showed as spaces.
- **Large batches no longer skip "complete" lines or the remote refresh.**
  When the interface fell behind a fast batch of small files, some
  finished jobs were logged without their file, or not at all, and an
  upload into the folder on screen could leave the remote pane stale.
- **A checkpoint can't grow too big to resume.** Queuing batch after
  batch while earlier ones ran could grow the checkpoint past what blink
  will load, so the transfer was saved but couldn't be resumed. A batch
  that would take it past the limit is now refused with a message, and
  the transfers already queued carry on.
- **Loading a large checkpoint uses far less memory.** It held several
  copies of the file in memory at once, enough on a big batch to stall
  blink or run out of memory.
- **Big queues stay responsive while an upload waits for its folder.**
  Each new job blink started rescanned every job that had finished since
  the upload began waiting. Starting a job now takes time in proportion to
  what is waiting, not to the size of the batch.
- **A retried or requeued download can't pick up a stray write.** A
  download that was cancelled, or failed partway, could still have its
  last write to the partial file in progress. Its retry, or a new job for
  the same file, measured the partial before that write landed and
  appended after it, and a corrupt file went into place as complete. The
  next attempt now waits for the earlier one's writes to finish before
  touching the partial. As a backstop, the partial's length is also
  checked before the rename: one of the wrong length is discarded, and the
  download fails.
- **A finished download survives a power cut.** The file was renamed into
  place without syncing its folder to disk, so a power cut could undo the
  rename after the checkpoint had already recorded the file as done, and a
  resume would skip it.
- **Cancelling can't miss a transfer that is just starting.** Cancelling
  a batch, or disconnecting, could catch a transfer the moment it started.
  It was then marked cancelled but kept running, another transfer could be
  queued onto the same file while it was still being written, and it was
  reported twice. A transfer cancelled the instant it started was also
  missed, and cancelling it said it "had already finished". All of these
  are now stopped.
- **Large batches can be resumed.** A batch of more than about 50,000
  files wrote a checkpoint bigger than blink would load, so neither the
  offer on connect nor `r` could resume it ("checkpoint file exceeds size
  limit"). Checkpoints are now written compactly, at about half the size,
  and the load limit fits the largest batch blink will plan.
- **Large batches no longer make the interface stutter.** The checkpoint
  was rewritten in full up to four times a second on the interface's own
  thread, tens of megabytes at a time for a large batch. Writes are now
  spaced out in proportion to how long they take.
- **Recursive uploads no longer fail on folders that aren't made yet.**
  A folder's mkdir is queued before the files that go in it, but with two
  or more parallel transfers (the default is two) they ran at the same
  time on different connections, and an upload often reached the server
  first and failed with "no such file". Nested folders could fail the same
  way. Uploads and mkdirs now wait for the folders above them.
- **Large SFTP batches no longer fail as lost connections on a slow
  link.** Each SFTP connection keeps 16 reads queued, and the SFTP library
  gives every read 10 seconds counted from when it is sent, including its
  time waiting in line. With several downloads sharing a link, the last
  read in line waited longer than that while data was still arriving. The
  read failed as a lost connection and the file was left as a
  `.blink-part`. A transfer now fails only after a minute with no reply,
  however long a read waits its turn. Uploads had the same problem and get
  the same fix.
- **A batch with failures says so when it ends.** Each failure left a
  single `failed:` line, which a large batch pushed out of the log long
  before it finished. The batch then ended on a run of `complete:` lines,
  which looked like success. A batch now ends with a line counting its
  failures, or confirming that every file completed.
- **The failed files of a finished batch can be retried.** `r` (or `R`
  for uploads) treated the failed entries still in the checkpoint as a
  batch in progress, and refused until you reconnected. It now re-queues
  just the failed files.
- **Disconnecting cancels running transfers, as its confirmation says.**
  It only stopped new jobs from starting: transfers already running carried
  on in the background on their own connections. And because job numbers
  restart with each connection, one of them finishing after you had
  reconnected could mark an unrelated job of the new connection as done in
  its checkpoint, so a later resume skipped that file. Running and queued
  transfers are now cancelled on disconnect (their checkpoint entries stay
  resumable), and transfer events from an earlier connection are ignored.
- **A stalled FTP transfer fails instead of hanging.** An FTP or FTPS
  download whose data connection went quiet without closing, or an upload
  the server stopped reading, waited forever and held its transfer slot.
  Both now fail after 60 seconds with nothing moving, as the file viewer
  already did.
- **An FTP reconnect can't freeze the remote pane.** After a dropped FTP or
  FTPS connection, blink reconnects before the next remote action, with no
  time limit: a server that accepted the connection and then said nothing
  held every remote action behind it. The reconnect now gets the same 30
  seconds as the first connect.
- **Batches of small files no longer wait between jobs.** With every
  transfer slot busy, blink checked for a free one only every 100 ms, so
  each slot sat idle for about 50 ms after every job: two minutes over
  10,000 small files on four slots. A finished job now starts the next one
  at once. Picking the next job also no longer rescans every finished one.
- **A crash in a background task no longer breaks the terminal.** blink's
  crash handler put the terminal back to normal for a panic on any thread,
  but a panic in a background task doesn't end blink, which kept running
  in a terminal no longer set up for it. Only a crash of the interface
  itself restores the terminal now; a background one goes to the log.
- **Quitting or disconnecting keeps the last moments of progress.**
  Checkpoints are written at most every quarter second, and the last
  write held back that way was dropped on quit or disconnect, so a resume
  redid those files. It's written on the way out now.
- **Cancelling a transfer that just finished says so**, instead of
  logging that it was cancelled.
- **The session list scrolls.** Sessions past the bottom of the terminal
  were never drawn, and the selection could move onto them out of sight.
  The list now follows the cursor, and a click selects the session drawn
  under it.
- **Sessions and checkpoints with long names save.** A session named past
  about 240 bytes couldn't be saved. 0.9.0 shortened checkpoint file names
  to fit, but every save also writes a temp file whose name adds about 20
  bytes to the real one, so a long session's checkpoint still failed.
  Session file names are now shortened the same way, the temp name is
  kept within the limit, and on Windows, control characters in a session
  name map to `_` in its file name.
- **`blink checkpoints --clean` removes checkpoints of an edited session.**
  Since 0.9.0 a checkpoint belongs to a session's name and server, so
  changing a saved session's host, port or username leaves its old
  checkpoints unoffered, but `--clean` kept them because the name still
  existed. They now count as orphaned.
- **A wrong key passphrase asks again.** Mistyping the passphrase of an
  OpenSSH-format or legacy PEM key failed the connect with "cryptographic
  error" or "padding error" instead of asking again, and an encrypted
  PKCS#8 key (`BEGIN ENCRYPTED PRIVATE KEY`) never asked at all. blink now
  tells these apart by russh's error type rather than its message text.

### Dependencies

- Added `rustls-native-certs` 0.8, to read the system's trust store for
  FTPS. It brings no crypto backend of its own.

- `russh` 0.64 -> 0.65, for RUSTSEC-2026-0495 (russh's agent server, which
  blink does not run). No changes outside the test server. It still pins
  `rsa` and `ssh-key` to the same release candidates.
- Lockfile refresh (`cargo update`). `russh` 0.65.2 drops its exact pin on
  the `rsa` release candidate, so `rsa` moves to 0.10.0, the release that
  fixes RUSTSEC-2023-0071 (the "Marvin Attack" timing sidechannel in RSA
  private-key operations). The `cargo audit` ignore for it and the README
  caveat about RSA client keys are removed. `ssh-key` is still a release
  candidate (0.7.0-rc.12), pinned by russh.
- Lockfile refresh (`cargo update`). `webpki-roots` 1.0.12 drops six trust
  roots (Buypass Class 2 and Class 3, Certigna Root CA, DigiCert Trusted
  Root G4, QuoVadis Root CA 1 G3 and 3 G3): an FTPS server whose
  certificate chains to one of them is now refused unless it is pinned.

## [0.9.0] — 2026-11-04

### Security

- **`@revoked` lines in known_hosts are honoured.** A key marked
  `@revoked` was skipped as an unreadable line, so another line trusting
  it still let the connection through. blink now refuses a revoked key
  outright, for every host and without a prompt, and logs why. The file
  also reads comma-separated host lists, `!`-negated names, and fields
  separated by tabs or several spaces, as OpenSSH writes them;
  `blink known-hosts remove` takes just the one name out of a list.
- **An IPv6 host can't borrow another address's stored key.** blink still
  reads the `host:port` form older versions wrote, but for an IPv6 address
  that form is ambiguous: `::1:22` is both `::1` on port 22 and `::1:22`.
  It is no longer read for IPv6 hosts; blink always bracketed those.
- **Windows: a server can no longer name a download after a device
  blink missed.** `CONIN$`, `CONOUT$`, `COM0`/`LPT0`, the ports written
  with superscript digits (`COM¹`), and names like `NUL .txt` (a space
  before the extension) now count as reserved, like `NUL` and `COM1`
  already did, and are skipped.
- **TLS and constant-time fixes from dependency updates.** rustls 0.23.46
  (RUSTSEC-2026-0346 to 0349, among them a key-exchange downgrade and a
  1 GiB pre-authentication allocation), rustls-webpki 0.103.16 and
  webpki-roots 1.0.11 (distrusted intermediates and emSign roots), all
  reached through FTPS; crypto-bigint 0.7.10 and ctutils 0.4.8 (timing
  leaks, RUSTSEC-2026-0387 and 0385) through SSH.

- **A known SSH host presenting a different kind of key is refused, not
  offered as new.** A host on file with, say, an Ed25519 key that presented
  an RSA key read as an unknown host, and got the ordinary "trust this
  host?" prompt rather than the changed-key refusal — so a man in the
  middle without the real key could be one habitual "accept" away from the
  connection. blink now asks first for the key types it has on file for the
  host, as OpenSSH does, so a server holding several keys proves itself
  with a known one; a known host that presents only a type not on file is
  refused as a changed key. If a server really did retire its old key
  type, `blink known-hosts remove <host>` and reconnect.
- **An FTP server can no longer point blink's data connections at another
  host.** In passive mode blink connected to whatever address the server's
  `227` reply named, so a hostile server — or, for plain FTP, anyone on
  the path — could aim them anywhere: a service on your own network would
  receive an upload's bytes, or be probed for open ports. blink now
  connects to the server it is already talking to, on the port the reply
  gives, as curl does by default. This also fixes servers behind NAT that
  advertise a private address. Applies to FTP and FTPS alike.
- **An SFTP server can no longer make blink allocate gigabytes with one
  packet header.** russh-sftp before 3.0.1 set aside a buffer as large as
  each incoming packet's 4-byte length field claimed — up to 4 GiB — before
  any of the packet arrived. 3.0.1 refuses packets over 256 KiB, which is
  larger than any reply blink asks for.

### Added

- **Mouse support.** Click to focus a pane and move its cursor;
  double-click a directory to enter it, or a file to transfer it — a
  remote file downloads, a local one uploads, just that file. The scroll
  wheel moves the cursor in the file panes, the transfer list and the
  session list, and scrolls the viewer; the bottom pane's tabs and
  transfers respond to clicks, and a session double-clicked connects.
  Clicks with Ctrl or Alt held are ignored, kept free for later.
  `mouse = false` under `[terminal]` in `config.ini` turns it off, leaving
  mouse capture off so the terminal's own text selection works. File panes
  now keep their scroll position while the cursor stays in view, instead
  of shifting with every move.

### Changed

- **Checkpoints record the server and account they were made for.** A
  checkpoint was keyed by session name alone, so `sftp://alice@host` and
  `sftp://bob@host` (both named `host`) shared one, as did a saved session
  edited to point elsewhere. Resuming an upload offered from one sent
  its files to the other. Checkpoints are now format version 4, carry an
  `origin` (`user@host:port`), and are offered, resumed and appended to
  only by a connection to that origin. `blink checkpoints` shows the
  origin. Checkpoints from earlier versions say nothing about their
  server, so they are no longer offered: `blink checkpoints` lists them as
  orphaned, and `blink checkpoints --clean` removes them with their
  partial downloads.

- **An interrupted download resumes only when the file provably hasn't
  changed.** The `.blink-part.meta` sidecar gains an `mtime` field, and a
  partial is continued only when both the size and the modification time
  match what the server reports now. A file replaced by another of the
  same size used to resume, joining the old file's head to the new one's
  tail. A size or mtime the server doesn't report (an FTP server without
  `SIZE` or `MDTM`) now means a restart rather than an unchecked resume.
  Partials left by earlier versions restart once.

- **Partial transfers are named `<name>.<hash>.blink-part`.** `<hash>` is
  8 hex digits of the SHA-256 of the file's name. A partial used to be
  plain `<name>.blink-part`, which is also the name of a real file called
  that: downloading `foo` beside a downloaded `foo.blink-part` deleted it
  as a stale partial, parallel downloads of the two wrote one file, and an
  upload of `foo` truncated a `foo.blink-part` on the server. A name too
  long to take the suffix within the usual 255-byte limit is now shortened
  in the partial's name, where its partial or `.meta` record used to fail
  to be created. Partial downloads left by earlier versions are not
  resumed; download those files again, and delete the old `.blink-part`
  and `.blink-part.meta` files by hand.

### Fixed

- **Windows: a failed ssh-agent login says why.** When the agent's keys
  were all refused, Windows reported only "no identity accepted", while
  Unix also gave the last error; and a refusal by Pageant's keys was
  reported as "no agent found". Both platforms now share one login loop
  and report the same way, naming Pageant when it was the agent used.
  The loop also stops offering keys once the server hangs up (as
  OpenSSH's `MaxAuthTries` does) instead of failing each remaining one.

- **Sessions with long names can be checkpointed again.** The checkpoint
  file was named after the whole session name, so a long one went past
  the file-name limit and the checkpoint could never be written. Names are
  now cut to fit. On Windows, control characters in a session name are
  also replaced, since Windows refuses them in file names.
- **A rename confirmed as an overwrite now overwrites over SFTP.** Renaming
  onto an existing name asked "overwrite?", then sent a plain SFTP rename,
  which OpenSSH's server refuses when the target exists, so the confirmed
  rename failed. It now uses the server's atomic replacing rename where
  offered, and otherwise removes the target first — only after checking
  the file being renamed is still there, so the target is never lost for
  nothing. Over FTP the rename is sent as before: whether it may replace
  a file is the server's setting.
- **A download whose server ends a file early mid-way now fails.** Parts
  of a download are fetched in parallel and written in order. If one came
  back short and later ones still had data, the rest was written straight
  after it, putting every following byte at the wrong place; the file
  could still come out the right size and be kept as complete. Such a
  download now fails instead, and nothing is renamed into place.
- **Two blink instances saving the same file no longer corrupt it.** The
  config, session, checkpoint and known_hosts files were each saved
  through one fixed temp file beside them, such as `config.tmp`. Two
  instances saving at once wrote into the same temp file, so a mix of
  both could be renamed into place, and the other save failed. Each save
  now writes its own temp file, removes it if the save fails, and never
  writes through a link left at its name.
- **Downloads left out for their names are now reported.** A remote file
  whose name can't be a local file name — one with a backslash in it, or
  on Windows a reserved name such as `NUL` — is skipped, but a recursive
  download said so only in the debug log, and a selected one vanished
  without a word. Both now give a log line with the count.
- **Local files whose names aren't valid UTF-8 are refused with the
  reason.** The local pane showed such a name with replacement characters,
  then used that altered name to upload, view or open it, which failed
  with "not found". They are now refused up front, saying why, as a
  directory upload already skipped them.
- **A host key trusted once is held to for the rest of the session.**
  "Trust once" accepted that exact key, and nothing else about the host:
  a transfer connection that later met a different key from the same
  host got the ordinary "new host" prompt, where a key stored in
  known_hosts would have been refused as changed. That second key is the
  man in the middle the first prompt guarded against. It is now refused
  as a changed key, and the warning says to reconnect, since there is no
  stored entry to remove.
- **Prompts raised by background work no longer take your keystrokes.**
  The overwrite question a recursive transfer asks, and the host-key
  prompt a transfer connection raises, opened whenever that work got
  there and answered on single letters. Typing a rename to `yearly.csv`
  as one opened answered "overwrite all" with the `y`, and the rename was
  lost; a host-key prompt could be accepted the same way. Opened over the
  file viewer or help, the host-key prompt also left Esc unable to leave
  either. Such a prompt now waits, with a log line saying so, until you
  are back at the file list, and ignores keys for a moment as it opens. A
  second recursive transfer's overwrite question also no longer replaces
  the first's unanswered: it is asked next.
- **Transfers prepared on one server are no longer started on the next.**
  A recursive upload or download first walks the tree into a list of
  transfers, and that walk kept running after a disconnect. If it
  finished once another server was connected, its list was queued there:
  local files went to paths chosen for the old server, overwriting files
  the conflict check had never seen, and a download fetched whatever the
  new server had at the old one's paths. A directory listing that outlived
  its connection could likewise fill the pane on the next server. Both are
  now dropped.
- **The file viewer no longer shows a file it was not opened on.** A fetch
  was matched to the viewer by file name alone, so one that outlived its
  viewer could fill a later viewer of a file with the same name: open a
  slow remote `README.md`, close it, open a local `README.md`, and the
  remote bytes arriving last replaced the local file's. Each opening of
  the viewer now only takes what was fetched for it.
- **A session whose name or path would not survive saving is refused, with
  the reason.** The session file format reads a value that opens with a
  quote as a quoted string, and trims spaces at either end, so such a
  value came back changed. A session named `"prod"` reloaded as `prod`,
  and since the file is named after the session, deleting or renaming it
  then missed the file it was really in. Saving now refuses a name, host,
  user, remote or local directory, or key path that starts with a quote or
  starts or ends with whitespace, instead of writing one that loads as
  something else. Session files written by hand load as before.
- **Viewing a text file larger than its listing said no longer freezes the
  UI.** The 1 MB text limit was checked against the size in the directory
  listing only, while a remote fetch is capped at 25 MB, the image limit.
  A listing that understated the size, or a file that grew after it was
  listed, could push all 25 MB through the text viewer's preparation on
  the UI thread. The limit is now checked against what arrives as well,
  and a local file is read only to one byte past it, so a FIFO or a file
  still growing cannot stall the viewer either.
- **The viewer's read-error message is sanitized like the rest.** When a
  preview failed, the error — which can quote the server's own reply —
  was shown in the viewer without the filtering every log line gets, so
  bidi overrides and zero-width characters could reach the screen.

### Dependencies

- `russh` 0.63 -> 0.64. No code changes. 0.64 removes the
  `legacy-ed25519-pkcs8-parser` feature, which blink never enabled, and
  still pins `rsa` and `ssh-key` to the same release candidates.
- Lockfile refresh (`cargo update`): `suppaftp` 12.1.1 -> 12.2.1, `tokio`
  1.53 -> 1.54 (RUSTSEC-2026-0386), and the security updates listed above.

## [0.8.2] — 2026-09-28

### Changed

- **A session file whose `name` or `remote_dir` holds a newline or NUL no
  longer loads.** The INI parser turns an escaped `\r\n` or `\0` in a
  value into the real character, and only `host` and `username` were
  checked on load, though saving already refused all four. Such a session
  loaded but could never be saved again; it is now listed as skipped, with
  the reason, like any other invalid session file.

- **A partial download's resume record now names the server it came
  from.** The `.blink-part.meta` sidecar gains an `origin` field,
  `user@host:port`. A record without one — written before this change —
  is treated as unidentified, so a download interrupted before upgrading
  restarts from the beginning once instead of resuming.

- **Partial transfers are named `<name>.blink-part`, not `<name>.part`.**
  Downloads in progress, their `.meta` resume records, and uploads in
  progress on the server all use the new suffix. Partial downloads left by
  earlier versions under `.part` are not resumed; download those files
  again, and delete the old `.part` and `.part.meta` files by hand.

### Fixed

- **An upload can't silently overwrite a file hidden in an unreadable FTP
  listing.** The overwrite check lists each destination directory and
  looks for the upload's name. When some of that listing's lines couldn't
  be read, a file of the same name could be among them, unseen, and be
  overwritten without asking. Uploads into such a directory now count as
  possible conflicts: the overwrite prompt appears, and the log explains
  why beforehand.

- **FTP listing lines blink can't read are no longer skipped silently.** A
  server whose directory listing lines match neither the POSIX nor the DOS
  format had them dropped with a warning only in the debug log, discarded
  by default: the pane came up short, or empty, with no reason given — and
  a recursive download quietly missed those entries while reporting
  success. The log pane now says how many entries of a directory couldn't
  be read (once, not on every refresh), and a download batch says how many
  it had to leave out.

- **A download can no longer resume from another server's partial file.**
  The resume record held only the remote path and size, so two servers —
  or two accounts on one — with a file at the same path and size,
  downloaded to the same place, could resume into each other's bytes.
  Resuming now also requires the same server, account and port.

- **blink no longer deletes or truncates other programs' `.part` files.**
  Its own partial files used the common `.part` suffix, so a
  `report.pdf.part` left by a browser beside a download of `report.pdf`
  was deleted or overwritten — and an upload did the same to a
  `data.csv.part` on the server. With the new suffix, blink only touches
  files it wrote.

- **FTP previews of large files work on slow links.** A preview's
  60-second deadline covered the whole transfer, so a large image could
  never preview below about 420 KB/s, however steadily it arrived. Only
  the FTP commands now carry that deadline; the file's data only has to
  keep arriving, and fails after 60 seconds with nothing new, after which
  the connection is reopened for the next command.

- **FTP downloads check that they received the whole file.** A download
  was renamed into place as soon as the server confirmed the transfer,
  whatever its length. A transfer cut short that the server still
  confirmed landed truncated; a resume the server acknowledged but ignored
  — sending the whole file again after the partial one — landed with its
  start duplicated. When the server reports a size, the download must now
  match it exactly, or it fails and its partial file is removed so the
  next attempt starts clean. SFTP already checked.

- **`blink connect` accepts email-address usernames.** The URL was split at
  its first `@`, so `ftp://user@example.com@files.host.net` connected as
  `user` to a host named `example.com@files.host.net`. It is now split at
  the last `@`, as curl and most URL parsers do.

- **Saving a host key no longer corrupts the last entry in `known_hosts`.**
  If the file did not end with a newline — after a hand edit, say — the
  new entry was written onto the end of its last line. That host's stored
  key then no longer matched, so its next connect was refused as a changed
  key, and the newly saved host was not found either. blink now ends the
  line first.

- **Opening an image no longer freezes blink.** Decoding, scaling and
  encoding a previewed image for the terminal ran on the UI thread, when it
  opened and again on every resize, so the whole interface — transfer
  progress included — stopped until it finished, which for a large image
  could take seconds. Rendering now runs in the background: the viewer
  says "rendering image…" until it is ready, a redraw at the same size
  reuses the result, and dragging a window's edge costs one render at a
  time rather than one per step. A render that fails says why in the
  viewer.

- **Tabs show up in the text viewer.** The viewer kept tab characters for
  the terminal to handle, but they never reached it: the rendering library
  drops control characters, tabs included, so tab-indented files —
  Makefiles, Go source — lost all their indentation. Tabs are now expanded
  to spaces at 8-column stops, as a terminal or `less` shows them.

- **Jobs that do not fit in a full transfer queue now run.** Past 100,000
  queued jobs, the rest of a batch was left pending in its checkpoint with
  nothing to run it, and the log said to press `r` once the queue drained —
  but `r` refused for the rest of the session, because the unfinished
  batch still counted as in flight. The overflow now waits in order and is
  queued automatically as earlier jobs finish; a new batch lines up behind
  it. Cancelling a batch with `C` cancels its waiting jobs too, and after a
  disconnect they stay in the checkpoint for the resume offer.

- **A large upload no longer floods the browsing connection with
  listings.** Every completed upload asked for a fresh listing of the
  remote pane, and each one queued behind the last on the connection the
  pane uses: an upload of a thousand files cost a thousand listings (a
  thousand data connections on FTP), and navigating or pressing F5 waited
  behind all of them. Uploads that landed outside the directory on screen
  triggered them too. Now a refresh asked for while that directory is
  already being listed folds into a single follow-up, so a batch costs one
  listing at a time; navigating away drops a follow-up for the directory
  left behind; and only uploads into the directory shown, or below it,
  refresh it.

- **Checking a host-key fingerprint no longer times the connection out.**
  The 30-second connect deadline covered the whole connect, including the
  time the host-key prompt waited for an answer, so anyone who took longer
  than about 30 seconds to verify a fingerprint through another channel —
  as the README advises — got "connection timed out" under the still-open
  prompt. The deadline now stops while the prompt is open. The prompt has
  its own limit, raised from 60 seconds to two minutes (OpenSSH's default
  `LoginGraceTime`); when it runs out, the log says the prompt timed out
  rather than blaming the server, and a prompt whose connection has failed
  is taken down instead of being left up.

- **Downloading a file that is already downloading no longer corrupts it.**
  Nothing stopped two jobs with one destination from running at once — a
  second Ctrl-D on the same file or folder, overlapping folder downloads,
  or on Windows and macOS a server directory holding both `README` and
  `readme`. Both wrote the same `.part`, and the first to finish renamed
  it into place and reported success while the other was still writing:
  the file came out short or with duplicated data. Reproduced against the
  test SFTP server with an 8 MiB file: 294 KB short or 522 KB too long,
  depending on timing. Uploads had the same flaw on the server side.

  A job is now not queued while another pending or running job writes the
  same destination — a download's local path, compared ignoring case on
  Windows and macOS, or an upload's remote path. The log says how many
  were skipped; two files in one batch that would land on the same path
  are each named, since one of them will not be transferred.

## [0.8.1] — 2026-09-26

### Security

- **An FTP server can no longer make blink buffer an endless reply.**
  suppaftp before 12.1 read each control-connection reply with no bound, so
  a server answering with a line that never ended — the greeting included,
  before any login — grew blink's memory until the connect deadline gave
  up. suppaftp 12.1 refuses a reply over 256 KiB, and the connection fails
  as soon as the cap is passed. Applies to FTP and FTPS alike.

  12.1 also checks where an active-mode data connection comes from; blink
  only uses passive mode, so that part changes nothing here.

### Fixed

- **A timed-out FTP preview no longer leaves browsing broken.** A preview
  whose 60-second deadline passed while its data connection was opening
  left suppaftp believing a data connection was still open, and every
  later listing or preview on that connection failed with "Data
  connection is already open" until the user reconnected by hand. suppaftp
  calls are not safe to cut short, so this was one case of a wider one:
  after any deadline, dropped call or lost connection, blink went on using
  a connection that could be out of step with the server. An FTP or FTPS
  connection now replaces itself instead: the call that broke it still
  fails, and the next call reconnects and logs in again before running.
  An FTPS reconnect must present the certificate the first connect
  accepted; it never pins a new one.

- **An FTP server hanging up now counts as a disconnect.** A control
  connection the server closed before replying surfaced as "Response
  contains an invalid syntax", and a `421` (the server closing the
  connection, often for idle time) as a plain error. Both are now
  reported as a lost connection. Besides the reconnect above, that makes
  the transfer queue retry a job whose pooled connection the server had
  reaped while idle, as it already did for SFTP.

## [0.8.0] — 2026-09-22

### Added

- **Plain FTP now says that it is unencrypted.** Every other protocol tells
  the user something about how it protects them — an unknown SSH host key
  prompts, an FTPS certificate gets pinned — while `ftp://` protected nothing
  and said nothing about it. Connecting over FTP now logs a warning, and the
  README's protocol list and security notes say the same thing. The wording is
  anchored on the connection rather than the credential, so it stays true of
  an anonymous login: there is no password worth stealing there, but the file
  names and contents are in the clear either way.

  No behaviour change — the user chose the protocol and everything keeps
  working.

### Fixed

- **The session selector no longer shows FTPS as being as risky as FTP.**
  `Protocol::Ftp | Protocol::Ftps` shared one arm of the protocol-tag
  colouring, both rendering in the theme's warning colour. That was wrong in
  both directions: it cautioned against the encrypted protocol, and it gave
  no way to tell the cleartext one apart from it at a glance. FTPS now reads
  like SFTP, and the warning colour is reserved for protocols where
  `Protocol::is_cleartext()` holds.

- **SFTP no longer lists sockets and block devices as directories.** The
  entry type was read with `russh_sftp`'s `is_dir()` / `is_symlink()`, which
  test whether the mode *contains* a type's bits rather than whether the
  type field equals it — and the POSIX type codes overlap, so a socket
  (`0o140000`) and a block device (`0o060000`) both passed as directories.
  They showed as folders in the file pane, and a recursive download or delete
  tried to open them as one and failed. They are now `Other`: downloaded as
  files, unlinked as leaves. The directory listing, `metadata`, and the
  recursive delete now share one classifier rather than three copies of it.

- **A failed FTP preview no longer breaks the connection it ran on.**
  Previews use the browsing connection. When one failed partway — a file
  over the preview cap, or a data connection that dropped mid-read — the
  transfer was never finalised, and every later listing or preview on that
  connection was refused with "Data connection is already open" until you
  reconnected. The over-cap case is fixed in blink; the dropped-connection
  case by suppaftp 12, whose transfers finalise themselves.

- **Hitting the FTP preview cap is no longer reported as a disconnect.** It
  read `Connection error: file exceeds preview size limit` and was classed
  as a dropped connection. It is now a plain transport error:
  `retr <path>: file exceeds preview size limit`.

- **FTPS downloads from strict TLS 1.3 servers no longer fail at the end.**
  suppaftp 11 sends a TLS `close_notify` before closing a download's data
  connection; some servers answered the abrupt close with
  `426 Transfer failed` after every byte had arrived.

### Security

- **The recursive remote delete is bounded on both transports.**
  `SftpTransport::delete_dir` and `ftp_delete_dir` grew their walk stack with
  no ceiling, so a server serving a deep or wide enough tree exhausted memory
  instead of drawing an error. Both now stop at `MAX_QUEUED_JOBS` — the same
  ceiling, and the same reason, as `walk_remote` — and refuse before removing
  anything, rather than half-deleting the tree and then failing. Symlinks were
  already treated as leaves in both walks, so there was never an infinite-loop
  risk, only an unbounded one.

- **The overwrite-confirmation modal sanitizes the remote name.** It was the
  one place a server-supplied name reached the screen without it. Not an
  injection — `ratatui`'s `Buffer::set_stringn` drops control characters and
  zero-width graphemes before they reach the terminal — but ratatui *deletes*
  those characters where blink *replaces them with a space*, so two remote
  names differing only by a bidi override rendered identically in the very
  prompt the user clears them through. That is the case `is_deceptive_format`
  was written for.

- **`validate_theme_name` rejects `:`.** On Windows a path component carrying
  a drive prefix but no root replaces the whole buffer, so
  `themes_dir().join("C:evil")` resolved outside the themes directory
  entirely. `safe_local_name_for` already documented and blocked the same
  hazard for server-supplied download names. Low reach — a theme name comes
  from the user's own config, not from a server — but the asymmetry was the
  kind that gets copied into the next validator.

- **`rustls` 0.23.43 -> 0.23.45, closing
  [RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285)**
  (medium, 5.3) — TLS 1.3 handshake messages were accepted across encryption
  level boundaries. Reached through `suppaftp` and `tokio-rustls`, so it sat
  in the path of every FTPS connection. Lockfile only; the manifest never
  named `rustls` directly.

- **`wnaf` 0.14.0 -> 0.14.1, off a yanked release.** Reached through
  `russh -> p256/p384/p521 -> primeorder`. No advisory, but a yanked crate in
  the graph is a signal not to sit on.

### Internal

- **The SFTP test harness can list directories.** Its `russh_sftp` handler
  implemented no directory operations at all, which left `delete_dir`, `list`
  and `mkdir` with no coverage on the SFTP side. It now serves `opendir`,
  `readdir` and `rmdir`, and refuses `rmdir` on a directory that still holds
  entries, so the walk's bottom-up order is enforced by the server rather than
  merely asserted. 430 tests -> 439.

- **RSA and ECDSA are covered, for host keys and client key auth.** The SFTP
  harness authenticated with a password only, so `AuthMethod::Key` had never
  run against a server, and the `rsa-sha2-512` negotiation the README lists
  as enforced was unreachable from any test. The harness now serves RSA and
  ECDSA host keys and verifies public-key auth; the keys are published test
  fixtures in `src/transport/sftp_test_keys.rs`, not secrets.

- **The sixel encoder is tested by a round trip.** Its output is decoded back
  and bounded against what was encoded (mean RGB error under 10/255), so an
  `icy_sixel` bump no longer ends in a manual look at a terminal.

- **The FTP harness answers like a daemon when a transfer is cut short.** An
  early-closed data connection now gets `426` with the control connection
  kept open, instead of ending the whole session; and a new fault resets the
  data connection mid-transfer, which is what exercises the preview fix
  above.

- **Formatting is rustfmt's defaults.** The tree was hand-wrapped and no
  configuration reproduced that wrapping, so it was reformatted once with a
  `rustfmt.toml` pinning the 2024 style. That commit is listed in
  `.git-blame-ignore-revs`; GitHub applies it automatically, and a local clone
  opts in with `git config blame.ignoreRevsFile .git-blame-ignore-revs`.
  Nothing enforces formatting, so run `cargo fmt` before committing.

- The suite stands at 447 tests.

### Changed

- **Semver-compatible sweep across the rest of the graph** (`cargo update`,
  40 packages). Notable: `russh` 0.63.1 -> 0.63.3, `clap` 4.6.6 -> 4.6.7,
  `smallvec` 1.15.2 -> 1.16.1, `syn` 3.0.4 -> 3.0.6. russh 0.63.3 drops its
  `internal-russh-num-bigint` fork for upstream `num-bigint` 0.5, taking the
  graph from 408 to 407 crates.

  The three majors held back from this sweep are taken separately; see
  Dependencies.

### Dependencies

- `russh-sftp` 2.4 -> 3.0, `suppaftp` 10 -> 12, `icy_sixel` 0.6 -> 0.7.
  Only suppaftp needed code, two lines. The sixel encoder is now checked by
  a round-trip test rather than by eye, so this bump did not end in a
  manual check.

### Documentation

- The README said some SFTP servers report a symlink with both the symlink
  and directory bits set. None can: the POSIX type codes make that
  combination impossible for a well-formed symlink. The Path-safety section
  now gives the real reason symlinks are leaves, and names sockets and
  devices alongside them.
- Third-party attributions corrected against each crate's own manifest.
  `webpki-roots` 1.x is CDLA-Permissive-2.0, not MPL-2.0; `icy_sixel` and
  `parking_lot` are MIT OR Apache-2.0, not MIT alone; `russh-sftp` is its
  own project (`AspectUnk/russh-sftp`), not part of `russh`; and `russh` and
  `icy_sixel` link to their current repositories.
- The viewer's scroll keys are documented, and the architecture tree lists
  `sftp_test_keys.rs`, so the private keys in the repository are explained
  where someone would find them.

### Known gaps

- FTPS still has no end-to-end test; the `close_notify` fix above rests on
  suppaftp's own tests.
- A preview cancelled while suppaftp is still opening its data connection can
  leave that connection refusing data commands until reconnect. It needs a
  control channel that stalls for a minute mid-open, and it is upstream
  behaviour; tracked in `docs/BACKLOG.md`.
- SFTP previews (`SftpTransport::read_to_bytes`) have no test; also in the
  backlog.

## [0.7.1] — 2026-08-29

### Fixed

- **`cargo build --target x86_64-pc-windows-gnu` failed on any machine without
  NASM.** `russh`'s default features pull `aws-lc-rs`, and `aws-lc-sys`
  assembles its Windows objects with NASM — a tool the README's MinGW
  instructions never mentioned. Linux takes a different assembly path, so a
  native build never noticed and the failure appeared only when
  cross-compiling. The Windows cross-compile now needs only `mingw-w64`.

### Changed

- **`russh` is built against `ring` rather than its default `aws-lc-rs`,**
  which removes `aws-lc-sys` from the dependency graph entirely (416 → 408
  crates) and is what fixes the Windows build above. The two are
  interchangeable implementations of the same SSH ciphers — russh's `ring` and
  `aws-lc-rs` branches import identical constants for AES-128/256-GCM and
  ChaCha20-Poly1305, and it refuses to compile with neither enabled. `ring`
  was already being built for TLS through `rustls`, so blink now compiles one
  crypto backend instead of two.

  Note the SFTP integration harness generates only Ed25519 keys, so the test
  suite does not exercise RSA or ECDSA against the swapped backend; the
  feature-level parity above is the evidence, not the test run.

  A test reads `Cargo.lock` and fails if `aws-lc` returns. russh only rejects
  *neither* backend being enabled, so any dependency that switches
  `aws-lc-rs` back on through feature unification would win silently — and
  break a target nobody builds daily.

- **Minimum supported Rust declared as 1.98**, replacing the 1.90 shipped in
  0.7.0. Both 1.89 and 1.90 were metadata claims: a declared `rust-version`
  records what a crate says about itself, not what it compiles at, and nothing
  here has ever been built below stable. 1.98 is the only floor anyone has
  verified. It is likely higher than the true floor, which is the honest
  direction to be wrong in — lowering it needs a real
  `cargo +<version> check --all-targets`, not another metadata walk.

### Documentation

- Added this changelog, starting at 0.7.0.
- Recorded that SSH host certificates are refused, and that FTP listings are
  parsed as POSIX or DOS only, in the feature list, security notes and
  caveats.
- Credited `ring`, now the crypto behind every connection, under its actual
  Apache-2.0 AND ISC license. Corrected the `tokio-rustls` and `tokio-util`
  credit rows: both were dropped as direct dependencies in 0.7.0 but still
  ship transitively, through `suppaftp` and `russh-sftp`.
- Corrected the `.cargo/audit.toml` rationale for RUSTSEC-2023-0071, which
  named a dependency path through russh's `internal-russh-forked-ssh-key`
  fork that russh 0.63 had already dropped.

## [0.7.0] — 2026-08-29

### Security

- **FTP control-channel injection via remote paths.** Every path blink sent
  came from a server listing or a session file, and nothing between there and
  the wire looked for a line terminator. suppaftp builds commands as
  `format!("RETR {p}")` and appends CRLF without escaping, so a path carrying
  CR or LF stopped being a path and became a second command. The case that
  matters is a shared server, where the sender is not the operator: another
  tenant names a file `x\r\nDELE //…`, and it fires the moment the victim
  lists or downloads that directory, executed with the victim's credentials.
  Remote paths carrying CR, LF, or NUL are now refused at the FTP boundary,
  with the operation named and the offending bytes sanitised out of the error.

- **The same injection reachable through a session URL.** `Session::from_url`
  validated `host` and `username` but never called `validate()`, which had
  always checked `remote_dir` — so the one field a session *file* could not
  carry was reachable through the URL that builds a session without one. The
  path is percent-decoded on the way in, so
  `ftp://user@host/pub%0D%0ADELE%20%2Fimportant.txt` produced a session whose
  `remote_dir` was `/pub\r\nDELE /important.txt`, sent on the first listing.

### Fixed

- **FTPS panicked on every connect.** `tokio-rustls` was declared with default
  features, which enabled rustls' `aws-lc-rs` backend alongside the `ring`
  backend already selected through suppaftp. rustls 0.23 refuses to guess
  between two providers, so `ClientConfig::builder()` panicked outright and
  the FTPS transport was unusable. blink now routes through suppaftp's own
  `rustls` re-export, so exactly one provider can reach rustls.

- **A failed `RETR` left `REST`'s offset set,** so the next download silently
  started mid-file and wrote a corrupt result. RFC 959 has the restart marker
  consumed by the next transfer command regardless of outcome.

- **Unparsable FTP listing lines became browser entries.** suppaftp's
  `File::from_str` falls back to the MLSX parser, which splits on `;` and
  names the file after the trailing token — so *any* non-empty line parsed, as
  a file named after the whole line, addressing nothing. blink issues `LIST`
  and never `MLSD`/`MLST`, so it now parses with the POSIX and DOS parsers
  only and skips what neither accepts.

- **`--clean` deleted checkpoints it could not vouch for.** Orphanhood was
  decided from `Session::list_all().unwrap_or_default()`, which read "I could
  not list your sessions" as "you have no sessions" — every checkpoint then
  looked orphaned and `--clean` quietly did the job of `--force`, sweeping
  resumable batches and their `.part` files while reporting each as
  "(orphaned)". A single unparseable `.ini` was enough to trigger it.

- **`parallel_downloads` had an accidental cliff at 255.** Both loaders parsed
  into `u8`: below the cliff, out-of-range values clamped; above it, the parse
  failed, so `parallel_downloads = 300` stopped blink starting from
  `config.ini` and was silently ignored in a session file. Both now parse into
  `u32` and clamp to the documented cap.

### Changed

- **SSH host certificates are refused, fail-closed.** russh 0.63 widened the
  host-key callback to surface certificates as well as keys. `known_hosts`
  maps a host to a literal key and blink has no `@cert-authority` support, so
  nothing can validate a certificate's CA signature, principals, or validity
  window; pinning one by its key would look like verification while checking
  none of that. The rejection reason is surfaced to the TUI rather than left
  as a generic connect failure. russh 0.60 could not surface a certificate to
  that callback at all, so no connection that worked before begins to fail.

- **Unparsable listing lines are now reported.** One `skipped N of M
  unparsable lines` warning per listing — aggregated per call rather than per
  line, because listing runs on interactive navigation.

- **Minimum supported Rust raised to 1.90.** The manifest declared 1.89 but
  had never been buildable at it: `quantette`, reached through `icy_sixel`,
  declares 1.90 and every published release does. (Superseded in 0.7.1 — see
  below.)

### Dependencies

- suppaftp 8 → 10, russh 0.60 → 0.63, sha2 0.10 → 0.11, base64 0.22 → 0.23,
  icy_sixel 0.5 → 0.6, ratatui → 0.30.2, crossterm → 0.29.
- Moved off the yanked chacha20 0.10.1; dropped the unused `tokio-util`
  dependency.
- russh 0.63 drops its `internal-russh-forked-ssh-key` fork for upstream
  `ssh-key`.

### Internal

- **An in-process FTP server harness with fault injection**, and roughly
  twenty integration tests over it. FTP was previously the least-tested
  transport: all six of its tests covered the path guard, and none exercised
  listing, transfer, or resume against a server. The harness was built and
  landed *before* the suppaftp bump so that upgrade could be measured rather
  than assumed — a malformed PASV octet panicked under 8.0.5 and returns an
  error under 10.0.2, observed on both sides.
- Config-directory test isolation, so tests can no longer reach or adopt the
  real user configuration.
- Test count 410 → 426.

### Known gaps

- FTPS has unit-test coverage only; `AUTH TLS` and certificate pinning are not
  exercised end to end.
- The FTP harness models a well-behaved server. Real-daemon dialects
  (vsftpd, IIS), NAT-mangled PASV replies, and TLS session reuse are uncovered.
- The MSRV floor is checked by a `cargo metadata` walk over declared
  `rust-version` fields, not by compiling at that version.
