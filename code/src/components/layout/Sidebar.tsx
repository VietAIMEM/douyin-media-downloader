import { NavLink } from "react-router-dom";

import { Icon as IconObject, type IconName } from "@/components/ui/icons";
import { NAV_GROUPS } from "@/navigation";

/** Which icon stands for each destination. */
const ITEM_ICONS: Record<string, IconName> = {
  "/": "home",
  "/single": "download",
  "/profile": "users",
  "/downloads": "downloadCloud",
  "/library": "library",
  "/dashboard": "folderOpen",
  "/history": "history",
  "/settings": "settings",
};

/** The icon component for a nav item, chosen by path. */
function NavIcon({ path }: { path: string }) {
  const Name = ITEM_ICONS[path] ?? "download";
  const Component = IconObject[Name];
  return <Component className="h-4 w-4 shrink-0" />;
}

/**
 * The application's navigation.
 *
 * The chrome reads darker than the workspace in both themes, so the window
 * reads as an application rather than a web page: one ground for where you are
 * and can go, another for what you are looking at. Each destination carries an
 * icon *and* a label — an icon-only rail hides its destinations behind hover
 * tooltips, which this window is too small for.
 */
export function Sidebar() {
  return (
    <nav
      aria-label="Main"
      className="flex w-60 shrink-0 flex-col border-r border-shell-border bg-shell"
    >
      <div className="flex items-center gap-2.5 px-5 py-5">
        <span
          aria-hidden
          className="flex h-8 w-8 items-center justify-center rounded-lg bg-brand text-white"
        >
          <IconObject.logo className="h-4 w-4" />
        </span>
        <span className="text-sm font-semibold leading-tight text-nav-primary">
          Douyin
          <br />
          Downloader
        </span>
      </div>

      <div className="scroll-pane flex-1 space-y-5 overflow-y-auto px-3 pb-2">
        {NAV_GROUPS.map((group) => (
          <div key={group.label}>
            <p className="px-2 pb-1 text-[10px] font-semibold uppercase tracking-widest text-nav-secondary">
              {group.label}
            </p>
            <ul className="space-y-0.5">
              {group.items.map((item) => (
                <li key={item.path}>
                  <NavLink
                    to={item.path}
                    end={item.path === "/"}
                    title={item.description}
                    className={({ isActive }) =>
                      `flex items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm transition-colors ${
                        isActive
                          ? "bg-nav-active-subtle font-medium text-nav-active"
                          : "text-nav-secondary hover:bg-shell-raised hover:text-nav-primary"
                      }`
                    }
                  >
                    <NavIcon path={item.path} />
                    {item.label}
                  </NavLink>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      <p className="px-5 py-4 text-[11px] leading-relaxed text-nav-secondary">
        Public content only.
        <br />
        Public Douyin links, never sign-in-gated media.
      </p>
    </nav>
  );
}
