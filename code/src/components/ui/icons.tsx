/**
 * The application's icons, in one place.
 *
 * The previous UI used unicode glyphs — ▶, ⚠, ✕, ↓ — whose rendering depended
 * on the fonts of the machine, and whose stroke weight could never match the
 * text they sat beside. One module now decides what each concept looks like, so
 * a screen never imports from the icon library directly and the glyph for
 * "video" cannot drift between screens.
 *
 * Every icon here is decorative: the accessible name comes from the control's
 * own text or `aria-label`, never from the icon. `aria-hidden` is applied here
 * rather than remembered at each call site.
 */
import {
  ArrowDown,
  ArrowLeft,
  CheckCircle2,
  ChevronLeft,
  ChevronRight,
  Clock,
  Database,
  Disc3,
  Download,
  DownloadCloud,
  ExternalLink,
  FileAudio,
  FileText,
  Film,
  Folder,
  FolderOpen,
  History,
  Home,
  Image as ImageIcon,
  Info,
  LayoutGrid,
  LibraryBig,
  ListMusic,
  List as ListIcon,
  Loader2,
  LogIn,
  LogOut,
  MonitorPlay,
  Music,
  RefreshCw,
  Search,
  Settings,
  SquareCheck,
  TriangleAlert,
  User,
  Users,
  X,
  Zap,
  type LucideIcon,
} from "lucide-react";
import type { ComponentType } from "react";

/** The shape every exported icon component shares. */
type Icon = ComponentType<{ className?: string }>;

/** Wrap a library icon so it is decorative wherever it lands. */
function make(IconComponent: LucideIcon): Icon {
  return function DecorativeIcon({ className = "h-4 w-4" }) {
    return <IconComponent aria-hidden="true" className={className} />;
  };
}

export const Icon = {
  logo: make(ArrowDown),
  back: make(ArrowLeft),
  check: make(CheckCircle2),
  chevronLeft: make(ChevronLeft),
  chevronRight: make(ChevronRight),
  clock: make(Clock),
  database: make(Database),
  disc: make(Disc3),
  download: make(Download),
  downloadCloud: make(DownloadCloud),
  external: make(ExternalLink),
  audio: make(FileAudio),
  text: make(FileText),
  video: make(Film),
  folder: make(Folder),
  folderOpen: make(FolderOpen),
  history: make(History),
  home: make(Home),
  image: make(ImageIcon),
  info: make(Info),
  /** A grid of cards — the Library's card layout. */
  layoutGrid: make(LayoutGrid),
  /** A list of rows — the Library's detail layout. */
  layoutList: make(ListIcon),
  library: make(LibraryBig),
  playlist: make(ListMusic),
  loading: make(Loader2),
  login: make(LogIn),
  logout: make(LogOut),
  watch: make(MonitorPlay),
  music: make(Music),
  refresh: make(RefreshCw),
  search: make(Search),
  settings: make(Settings),
  selectAll: make(SquareCheck),
  warning: make(TriangleAlert),
  user: make(User),
  users: make(Users),
  close: make(X),
  zap: make(Zap),
};

export type IconName = keyof typeof Icon;
