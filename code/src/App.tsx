import { useEffect, useState, type ReactElement } from "react";
import { HashRouter, useLocation } from "react-router-dom";

import { AppLayout } from "@/components/layout/AppLayout";
import { PageBoundary } from "@/components/layout/PageBoundary";
import { DashboardPage } from "@/pages/DashboardPage";
import { DownloadsPage } from "@/pages/DownloadsPage";
import { LibraryPage } from "@/pages/LibraryPage";
import { HistoryPage } from "@/pages/HistoryPage";
import { HomePage } from "@/pages/HomePage";
import { NotFoundPage } from "@/pages/NotFoundPage";
import { ProfileDownloaderPage } from "@/pages/ProfileDownloaderPage";
import { SettingsPage } from "@/pages/SettingsPage";
import { SingleUrlPage } from "@/pages/SingleUrlPage";
import { useBackendStore } from "@/stores/useBackendStore";
import { startDownloadEvents } from "@/stores/useDownloadStore";
import { useSettingsStore } from "@/stores/useSettingsStore";

/** Every screen, and the path that shows it. */
const SCREENS: { path: string; title: string; element: ReactElement }[] = [
  { path: "/", title: "Home", element: <HomePage /> },
  { path: "/profile", title: "Profile Downloader", element: <ProfileDownloaderPage /> },
  { path: "/single", title: "Single URL", element: <SingleUrlPage /> },
  { path: "/downloads", title: "Downloads", element: <DownloadsPage /> },
  { path: "/library", title: "Library", element: <LibraryPage /> },
  { path: "/dashboard", title: "Download Folder", element: <DashboardPage /> },
  { path: "/history", title: "History", element: <HistoryPage /> },
  { path: "/settings", title: "Settings", element: <SettingsPage /> },
];

/**
 * The screens, mounted once and kept.
 *
 * Switching tabs used to destroy the screen you left: the Single URL box and its
 * resolved preview, the profile you had loaded and every post you had selected,
 * the page of a listing you were part-way through — all of it lived in component
 * state, and React Router unmounts the route it leaves. Coming back meant
 * starting again, and a download you had just submitted appeared to have been
 * forgotten because the only copy of its task id went with the screen.
 *
 * So each screen is mounted the first time it is visited and then hidden rather
 * than unmounted. It is the smallest change that keeps a working session intact,
 * and it cannot forget a piece of state the way lifting fields into a store one
 * by one can.
 *
 * What this deliberately does not do is keep anything expensive or unsafe alive.
 * Screens hold text, selections and ids; they do not own browsers, sockets or
 * downloads. A download runs in the backend whatever this does, and a browser
 * session belongs to the renderer and is closed by its own idle timer — see
 * `network/chromium`. Screen lifetime and task lifetime are separate, and this
 * only touches the first.
 */
function Screens() {
  const { pathname } = useLocation();
  const [visited, setVisited] = useState<string[]>([]);

  useEffect(() => {
    setVisited((previous) =>
      previous.includes(pathname) ? previous : [...previous, pathname],
    );
  }, [pathname]);

  const known = SCREENS.some((screen) => screen.path === pathname);
  const starts = (screen: string) => pathname === screen || pathname.startsWith(`${screen}/`);

  return (
    <AppLayout>
      {SCREENS.map(({ path, title, element }) => {
        if (!visited.includes(path)) return null;
        const active = path === "/" ? pathname === "/" : starts(path);
        return (
          // Hidden rather than not rendered: the element stays mounted, so its
          // state, its scroll position and its subscriptions survive. A class
          // rather than the `hidden` attribute, because a class wins against
          // anything a stylesheet might say about divs.
          <div key={path} className={active ? undefined : "hidden"}>
            <PageBoundary title={title}>{element}</PageBoundary>
          </div>
        );
      })}
      {!known && <NotFoundPage />}
    </AppLayout>
  );
}

/**
 * Application root.
 *
 * The two startup reads — backend diagnostics and stored settings — happen once
 * here rather than per screen, so every screen renders against the same
 * snapshot and the theme can be applied as soon as settings arrive.
 *
 * `HashRouter` rather than `BrowserRouter`: the frontend is served by Tauri's
 * asset protocol from a single document, so a path-based route would 404 on
 * reload. Hash routing keeps every route inside the one document.
 */
export function App() {
  const refreshBackend = useBackendStore((state) => state.refresh);
  const loadSettings = useSettingsStore((state) => state.load);

  useEffect(() => {
    void refreshBackend();
    void loadSettings();
    // The download stream is subscribed to exactly here, once, for the life of
    // the application. A screen subscribing for itself is how a task that
    // finished while you were elsewhere went unnoticed, and how two screens
    // ended up with different ideas about the same task.
    return startDownloadEvents();
  }, [refreshBackend, loadSettings]);

  return (
    <HashRouter>
      <Screens />
    </HashRouter>
  );
}
