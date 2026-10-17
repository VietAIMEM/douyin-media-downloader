/**
 * The application's navigation model.
 *
 * Declared once so the sidebar and the router cannot drift apart: adding a
 * screen means adding one entry plus one route. Each entry names the sidebar
 * group it sits under, and the sidebar draws one heading per group.
 */

export interface NavItem {
  path: string;
  label: string;
  /** Shown as the page subtitle and as the link tooltip. */
  description: string;
  /** Sidebar group; screens in the same group draw under one heading. */
  group: "Library" | "Download" | "App";
}

/**
 * The destinations.
 *
 * The previous sidebar listed eight items in a flat row, with Home and
 * Dashboard both reading as "the overview" and Dashboard's title promising a
 * statistics page that never was. The grouping says what each screen is *for*:
 * getting more media, finding it again, or the application itself.
 *
 * Dashboard keeps its route and its data semantics — a filesystem scan, not
 * the database — and is renamed to say what it is: the download folder,
 * browsed directly.
 */
export const NAV_ITEMS: NavItem[] = [
  {
    path: "/",
    label: "Home",
    description: "Start a download, see what is running, find what you saved",
    group: "App",
  },
  {
    path: "/single",
    label: "Single URL",
    description: "Download one video, image post, or carousel",
    group: "Download",
  },
  {
    path: "/profile",
    label: "Profile Downloader",
    description: "Browse a creator's public posts and download a selection",
    group: "Download",
  },
  {
    path: "/downloads",
    label: "Downloads",
    description: "The queue: what is running, waiting, finished or failed",
    group: "Download",
  },
  {
    path: "/library",
    label: "Library",
    description: "Search, browse and download again from everything discovered",
    group: "Library",
  },
  {
    path: "/dashboard",
    label: "Download Folder",
    description:
      "What is actually on disk, folder by folder — including files put there by hand",
    group: "Library",
  },
  {
    path: "/history",
    label: "History",
    description: "Every download that finished, succeeded or not",
    group: "App",
  },
  {
    path: "/settings",
    label: "Settings",
    description: "Download folder, file naming, and preferences",
    group: "App",
  },
];

/** Sidebar order: Download, Library, then App. */
export const NAV_GROUPS: { label: string; items: NavItem[] }[] = (
  ["Download", "Library", "App"] as const
).map((group) => ({
  label: group,
  items: NAV_ITEMS.filter((item) => item.group === group),
}));

/**
 * Look up a nav item by path prefix, so a deep link under a screen still
 * highlights its parent.
 */
export function navForPath(pathname: string): NavItem | undefined {
  return (
    NAV_ITEMS.find((item) => item.path === pathname) ??
    NAV_ITEMS.find(
      (item) => item.path !== "/" && pathname.startsWith(`${item.path}/`),
    )
  );
}
