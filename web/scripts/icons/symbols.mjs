// The Material Symbols this app vendors, and how they are drawn.
//
// Add an icon: put its name here (copy the exact name from
// https://fonts.google.com/icons), then run `npm run icons`.
// Remove one the same way — the generator only emits what is listed.

/** Symbol names, outlined style, weight 400. Keep sorted. */
export const SYMBOLS = [
  "add", // was Lucide Plus
  "air", // was Lucide Wind
  "album", // was Lucide Disc
  "archive", // was Lucide Archive
  "arrow_back", // was Lucide ArrowLeft
  "arrow_downward", // was Lucide ArrowDown
  "attach_money", // was Lucide DollarSign
  "auto_awesome", // was Lucide Sparkles
  "back_hand", // was Lucide Hand
  "battery_android_frame_1",
  "battery_android_frame_2", // was Lucide BatteryLow
  "battery_android_frame_4",
  "battery_android_frame_bolt", // was Lucide BatteryCharging
  "battery_android_frame_full", // was Lucide BatteryMedium
  "battery_full_alt", // was Lucide Battery/BatteryFull
  "bluetooth", // was Lucide Bluetooth
  "bolt", // was Lucide Zap
  "brush", // was Lucide Paintbrush
  "build", // was Lucide Wrench
  "cached", // was Lucide RefreshCw
  "calendar_month", // was Lucide Calendar
  "cancel", // was Lucide XCircle
  "cardiology", // was Lucide HeartPulse
  "charger", // was Lucide PlugZap
  "check", // was Lucide Check
  "check_box", // was Lucide CheckSquare
  "check_circle", // was Lucide CheckCircle/CheckCircle2
  "chevron_left", // was Lucide ChevronLeft
  "chevron_right", // was Lucide ChevronRight
  "close", // was Lucide X
  "close_fullscreen", // was Lucide Minimize2
  "cloud", // was Lucide Cloud
  "cloud_off", // was Lucide CloudOff
  "contact_support", // was Lucide MessageCircleQuestion
  "content_copy", // was Lucide Copy
  "conversion_path", // was Lucide Route
  "create_new_folder", // was Lucide FolderPlus
  "dashboard", // was Lucide LayoutDashboard
  "data_usage",
  "delete", // was Lucide Trash2
  "description", // was Lucide FileText
  "device_thermostat", // was Lucide Thermometer
  "directions_car", // was Lucide Car
  "download", // was Lucide Download
  "draft", // was Lucide File
  "edit", // was Lucide Pencil
  "electrical_services", // was Lucide Cable
  "encrypted", // was Lucide FileLock2
  "error", // was Lucide AlertCircle
  "ev_station", // was Lucide EvCharger
  "expand_less", // was Lucide ChevronUp
  "expand_more", // was Lucide ChevronDown
  "filter_alt", // was Lucide Filter
  "first_page", // was Lucide ChevronFirst
  "folder", // was Lucide Folder
  "folder_open", // was Lucide FolderOpen
  "fullscreen", // was Lucide Maximize
  "fullscreen_exit", // was Lucide Minimize
  "gpp_maybe", // was Lucide ShieldAlert
  "group", // was Lucide Users
  "hard_drive", // was Lucide HardDrive
  "home",
  "info", // was Lucide Info
  "key", // was Lucide Key
  "lan", // was Lucide EthernetPort
  "last_page", // was Lucide ChevronLast
  "layers", // was Lucide Layers
  "line_end",
  "line_start",
  "local_fire_department", // was Lucide Flame
  "location_on", // was Lucide MapPin
  "lock", // was Lucide Lock
  "login", // was Lucide LogIn
  "logout", // was Lucide LogOut
  "memory", // was Lucide Cpu
  "menu", // was Lucide Menu
  "movie", // was Lucide Film
  "music_note", // was Lucide Music/Music2
  "nest_eco_leaf", // was Lucide Leaf
  "notifications", // was Lucide Bell
  "notifications_active", // was Lucide BellRing
  "notifications_off", // was Lucide BellOff
  "open_in_full", // was Lucide Maximize2
  "open_in_new", // was Lucide ExternalLink
  "pause", // was Lucide Pause
  "photo_camera", // was Lucide Camera
  "play_arrow", // was Lucide Play
  "power", // was Lucide Plug
  "power_off", // was Lucide Unplug
  "power_settings_new", // was Lucide Power
  "progress_activity", // was Lucide Loader2
  "receipt_long", // was Lucide ScrollText
  "rectangle", // was Lucide RectangleHorizontal
  "rotate_left", // was Lucide RotateCcw
  "rotate_right", // was Lucide RotateCw
  "save", // was Lucide Save
  "schedule", // was Lucide Clock
  "search", // was Lucide Search
  "sell", // was Lucide Tag
  "send", // was Lucide Send
  "sensors", // was Lucide Radio
  "settings", // was Lucide Settings/Cog
  "settings_remote",
  "shield", // was Lucide Shield
  "shuffle", // was Lucide Shuffle
  "skip_next", // was Lucide SkipForward
  "skip_previous", // was Lucide SkipBack
  "smart_toy", // was Lucide Bot
  "speed", // was Lucide Gauge
  "stethoscope", // was Lucide Stethoscope
  "straighten", // was Lucide Ruler
  "swap_vert", // was Lucide ArrowUpDown
  "task", // was Lucide FileCheck2
  "terminal_2", // was Lucide Terminal/TerminalSquare
  "timer", // was Lucide Timer
  "travel", // was Lucide Plane
  "trending_up", // was Lucide TrendingUp
  "tune", // was Lucide Settings2
  "upload", // was Lucide Upload
  "upload_file", // was Lucide HardDriveUpload
  "usb", // was Lucide Usb
  "verified_user", // was Lucide ShieldCheck
  "videocam", // was Lucide Video
  "visibility", // was Lucide Eye
  "visibility_off", // was Lucide EyeOff
  "vital_signs", // was Lucide Activity
  "volume_up", // was Lucide Volume2
  "wand_stars", // was Lucide Wand2
  "warning", // was Lucide AlertTriangle
  "webhook", // was Lucide Webhook
  "wifi", // was Lucide Wifi
  "wifi_off", // was Lucide WifiOff
]

/**
 * Symbols mirrored horizontally when drawn. Material draws its horizontal
 * batteries terminal-left; this app shows them terminal-right, matching the
 * battery_android_frame_* family which already points that way.
 */
export const FLIP = new Set(["battery_full_alt"])

/**
 * Which Lucide icon each symbol replaced, kept as provenance in the generated
 * doc comments. Historical only — new symbols do not need an entry.
 */
export const LUCIDE_ORIGIN = {
  "error": [
    "AlertCircle"
  ],
  "warning": [
    "AlertTriangle"
  ],
  "info": [
    "Info"
  ],
  "check": [
    "Check"
  ],
  "check_circle": [
    "CheckCircle",
    "CheckCircle2"
  ],
  "check_box": [
    "CheckSquare"
  ],
  "close": [
    "X"
  ],
  "cancel": [
    "XCircle"
  ],
  "progress_activity": [
    "Loader2"
  ],
  "shield": [
    "Shield"
  ],
  "gpp_maybe": [
    "ShieldAlert"
  ],
  "verified_user": [
    "ShieldCheck"
  ],
  "auto_awesome": [
    "Sparkles"
  ],
  "cardiology": [
    "HeartPulse"
  ],
  "vital_signs": [
    "Activity"
  ],
  "stethoscope": [
    "Stethoscope"
  ],
  "expand_more": [
    "ChevronDown"
  ],
  "expand_less": [
    "ChevronUp"
  ],
  "chevron_left": [
    "ChevronLeft"
  ],
  "chevron_right": [
    "ChevronRight"
  ],
  "first_page": [
    "ChevronFirst"
  ],
  "last_page": [
    "ChevronLast"
  ],
  "arrow_back": [
    "ArrowLeft"
  ],
  "arrow_downward": [
    "ArrowDown"
  ],
  "swap_vert": [
    "ArrowUpDown"
  ],
  "menu": [
    "Menu"
  ],
  "search": [
    "Search"
  ],
  "open_in_new": [
    "ExternalLink"
  ],
  "dashboard": [
    "LayoutDashboard"
  ],
  "fullscreen": [
    "Maximize"
  ],
  "open_in_full": [
    "Maximize2"
  ],
  "fullscreen_exit": [
    "Minimize"
  ],
  "close_fullscreen": [
    "Minimize2"
  ],
  "login": [
    "LogIn"
  ],
  "logout": [
    "LogOut"
  ],
  "directions_car": [
    "Car"
  ],
  "battery_full_alt": [
    "Battery",
    "BatteryFull"
  ],
  "battery_android_frame_full": [
    "BatteryMedium"
  ],
  "battery_android_frame_2": [
    "BatteryLow"
  ],
  "battery_android_frame_bolt": [
    "BatteryCharging"
  ],
  "ev_station": [
    "EvCharger"
  ],
  "electrical_services": [
    "Cable"
  ],
  "charger": [
    "PlugZap"
  ],
  "bolt": [
    "Zap"
  ],
  "power": [
    "Plug"
  ],
  "power_off": [
    "Unplug"
  ],
  "power_settings_new": [
    "Power"
  ],
  "speed": [
    "Gauge"
  ],
  "nest_eco_leaf": [
    "Leaf"
  ],
  "attach_money": [
    "DollarSign"
  ],
  "local_fire_department": [
    "Flame"
  ],
  "device_thermostat": [
    "Thermometer"
  ],
  "air": [
    "Wind"
  ],
  "conversion_path": [
    "Route"
  ],
  "location_on": [
    "MapPin"
  ],
  "album": [
    "Disc"
  ],
  "hard_drive": [
    "HardDrive"
  ],
  "upload_file": [
    "HardDriveUpload"
  ],
  "usb": [
    "Usb"
  ],
  "draft": [
    "File"
  ],
  "description": [
    "FileText"
  ],
  "task": [
    "FileCheck2"
  ],
  "encrypted": [
    "FileLock2"
  ],
  "folder": [
    "Folder"
  ],
  "folder_open": [
    "FolderOpen"
  ],
  "create_new_folder": [
    "FolderPlus"
  ],
  "archive": [
    "Archive"
  ],
  "download": [
    "Download"
  ],
  "upload": [
    "Upload"
  ],
  "save": [
    "Save"
  ],
  "content_copy": [
    "Copy"
  ],
  "delete": [
    "Trash2"
  ],
  "layers": [
    "Layers"
  ],
  "play_arrow": [
    "Play"
  ],
  "pause": [
    "Pause"
  ],
  "skip_previous": [
    "SkipBack"
  ],
  "skip_next": [
    "SkipForward"
  ],
  "shuffle": [
    "Shuffle"
  ],
  "videocam": [
    "Video"
  ],
  "movie": [
    "Film"
  ],
  "photo_camera": [
    "Camera"
  ],
  "music_note": [
    "Music",
    "Music2"
  ],
  "volume_up": [
    "Volume2"
  ],
  "rectangle": [
    "RectangleHorizontal"
  ],
  "brush": [
    "Paintbrush"
  ],
  "wand_stars": [
    "Wand2"
  ],
  "wifi": [
    "Wifi"
  ],
  "wifi_off": [
    "WifiOff"
  ],
  "bluetooth": [
    "Bluetooth"
  ],
  "lan": [
    "EthernetPort"
  ],
  "cloud": [
    "Cloud"
  ],
  "cloud_off": [
    "CloudOff"
  ],
  "sensors": [
    "Radio"
  ],
  "webhook": [
    "Webhook"
  ],
  "send": [
    "Send"
  ],
  "settings": [
    "Settings",
    "Cog"
  ],
  "tune": [
    "Settings2"
  ],
  "build": [
    "Wrench"
  ],
  "memory": [
    "Cpu"
  ],
  "terminal_2": [
    "Terminal",
    "TerminalSquare"
  ],
  "cached": [
    "RefreshCw"
  ],
  "rotate_left": [
    "RotateCcw"
  ],
  "rotate_right": [
    "RotateCw"
  ],
  "key": [
    "Key"
  ],
  "lock": [
    "Lock"
  ],
  "visibility": [
    "Eye"
  ],
  "visibility_off": [
    "EyeOff"
  ],
  "smart_toy": [
    "Bot"
  ],
  "contact_support": [
    "MessageCircleQuestion"
  ],
  "notifications": [
    "Bell"
  ],
  "notifications_off": [
    "BellOff"
  ],
  "notifications_active": [
    "BellRing"
  ],
  "schedule": [
    "Clock"
  ],
  "timer": [
    "Timer"
  ],
  "calendar_month": [
    "Calendar"
  ],
  "filter_alt": [
    "Filter"
  ],
  "sell": [
    "Tag"
  ],
  "add": [
    "Plus"
  ],
  "edit": [
    "Pencil"
  ],
  "travel": [
    "Plane"
  ],
  "group": [
    "Users"
  ],
  "back_hand": [
    "Hand"
  ],
  "straighten": [
    "Ruler"
  ],
  "receipt_long": [
    "ScrollText"
  ],
  "trending_up": [
    "TrendingUp"
  ]
}
