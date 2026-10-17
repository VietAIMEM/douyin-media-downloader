# Support and reporting a problem

## Where to report

**This project has no public issue tracker configured yet.** That is a release
owner action, not something this document can invent: until a tracker, a support
address, or a discussion forum exists, there is nowhere for a report to go, and
inventing an address would send people to a page nobody runs.

Until then, the procedure below is what a report has to contain, whoever it is
sent to.

---

## Before you report

Most problems in this application are one of a handful of things, and the answer
is often quicker than a report:

- [Troubleshooting](TROUBLESHOOTING.md) covers every message the application
  produces.
- [FAQ](FAQ.md) covers the questions that come up most: why a page cannot be
  read, why a download started over, why FFmpeg or a browser is needed.

---

## What to include

1. **The version.** The Home screen shows it, and the Settings screen shows it
   under **Updates**. A report against the wrong version wastes everyone's time.
2. **What you did**, in order, and what happened. "Analyzed a profile link, picked
   three posts, submitted, the second one failed" is more useful than "it's
   broken".
3. **The exact message**, copied rather than paraphrased. The wording is chosen
   per failure and identifies which check failed.
4. **The Home screen's diagnostics**: whether the database is healthy, whether
   browser-assisted resolution is available, and whether media muxing (FFmpeg) is
   available.
5. **The log**, with the redaction below.

---

## The log, and what to remove from it

Logs are at:

```
%APPDATA%\DouyinDownloader\DouyinDownloader\data\logs\
```

The most recent file is the one that matters. Set `RUST_LOG=debug` before
launching, reproduce the problem, and then take the file — debug detail is
usually the difference between a guess and a diagnosis.

**Read it before you send it.** The application is careful about what it writes,
but a log is written by you and is yours to check.

Already excluded by design:

- **Signed media URLs.** Only the scheme, host and path are ever written; the
  query string — which for a Douyin media URL is a short-lived credential — is
  never written, anywhere. This is enforced in one place and covered by tests.
- **Credentials of any kind.** The application has none to write.
- **Page contents.**
- **Anything from your account.** There is no account.

Still present, and worth removing if you would rather not share it:

| In the log | Why it is there | What to do |
| --- | --- | --- |
| Your Windows user name, inside file paths | The download folder and the data folder are yours | Replace your user name with `%USER%`. |
| The ids of posts and creators you downloaded | They identify which transfer a line belongs to | Remove the lines, or replace the ids. They are public ids, but they say what you downloaded. |
| Host names the application contacted | Which CDN refused a transfer is the useful part of a failure | Leave them; they are Douyin's, not yours. |

Do **not** send:

- the database file — it contains your whole Library and download history;
- a list of your downloaded files;
- anything from `%APPDATA%\DouyinDownloader\..\data\douyin-downloader.db-*`.

A short, specific log extract beats the whole file.

---

## What you will be asked for most often

**Something failed and the message was vague.** The log line for that failure —
filter the file for `ERROR` or `WARN`.

**A download will not complete.** The Home screen's FFmpeg and browser status,
then the log lines for that task id.

**The application will not start.** The last log file, or, if there is not one,
anything it printed when launched from a terminal:

```
"%LOCALAPPDATA%\Douyin Downloader\douyin-downloader.exe"
```

---

## What this document does not promise

There is no crash reporting and no automatic upload of anything, so a problem you
do not report is a problem nobody knows about. See
[Privacy](PRIVACY.md) for why that is the deliberate choice — and for what would
have to change before any automatic reporting were added.
