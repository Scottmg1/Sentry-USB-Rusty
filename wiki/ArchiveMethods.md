# Archive Methods

Sentry USB can automatically back up your Tesla dashcam clips to a Network Attached Storage (NAS) or Cloud Storage whenever the Pi connects to WiFi. Choose the method that best fits your setup.

## Overview

| Method | Best For | Requires |
|--------|----------|----------|
| **CIFS / SMB** | Windows/Mac file shares, NAS devices | Server hostname, share name, credentials |
| **rsync** | Linux servers, another Raspberry Pi | SSH access with key-based auth |
| **rclone** | Cloud storage (Google Drive, S3, Dropbox, etc.) | SSH session for `rclone config` |
| **NFS** | Linux/NAS with NFS exports | NFS server and export path |
| **None** | No backup — clips stay on the Pi | Nothing |

All methods are configured through the **Setup Wizard** → **Archive** step. Some methods (rsync, rclone) require additional SSH setup after the wizard.

---

## CIFS / SMB (Windows/Mac File Sharing)

The simplest archive method. Works with Windows shares, macOS Sharing, Samba on Linux, and most NAS devices.

### What You Need
- A shared folder on your network (e.g., on a Windows PC, Mac, or NAS)
- Username and password with read/write access to the share

### Wizard Fields
| Field | Description |
|-------|-------------|
| **Archive Server** | Hostname or IP address of the file server |
| **Share Name** | Name of the shared folder |
| **Username** | User with access to the share |
| **Password** | Password for the share user |
| **Domain** | Optional — only needed for domain-joined Windows environments |
| **CIFS Version** | Optional — defaults to `3.0`. Use `2.0` or `1.0` for older servers. |

---

## rsync (SSH-based File Sync)

Efficient incremental file sync over SSH. Good for Linux servers or another Raspberry Pi with an external drive.

### What You Need
- A server running SSH with rsync installed
- SSH key-based authentication (set up after the wizard)

### Wizard Fields
| Field | Description |
|-------|-------------|
| **Server** | Hostname or IP of the archive server |
| **Username** | SSH user on the server |
| **Remote Path** | Destination directory (e.g., `/mnt/storage/SentryArchive/`) |

### Post-Wizard SSH Setup
After the wizard completes, you must set up SSH keys:

```bash
ssh pi@sentryusb.local
sudo -i
/root/bin/remountfs_rw
ssh-keygen
ssh-copy-id user@archiveserver
```

---

## rclone (Cloud Storage)

Archive to any cloud provider supported by [rclone](https://rclone.org/) — Google Drive, Amazon S3, Dropbox, OneDrive, Backblaze B2, and [many more](https://rclone.org/#providers).

### What You Need
- An account on a supported cloud storage service
- SSH access to the Pi (rclone requires interactive terminal setup)

### Wizard Fields
| Field | Description |
|-------|-------------|
| **Remote Name** | Name you'll use for the rclone remote (e.g., `gdrive`) |
| **Remote Path** | Folder on the remote for clips (e.g., `SentryArchive`) |
| **Archive Server** | IP address for connectivity checks (e.g., `8.8.8.8`) |

### Post-Wizard SSH Setup
After the wizard completes, install and configure rclone:

```bash
ssh pi@sentryusb.local
sudo -i
/root/bin/remountfs_rw
curl https://rclone.org/install.sh | sudo bash
rclone config    # Use the same remote name you entered in the wizard
rclone mkdir "remotename:SentryArchive"
```

---

## Network File System (NFS)

Direct NFS mount. Common on Linux servers and many NAS devices (Synology, QNAP, Unifi, UGreen etc.).

### What You Need
- An NFS server with an exported path
- The Pi must be able to reach the NFS server on the network

### Wizard Fields
| Field | Description |
|-------|-------------|
| **NFS Server** | Hostname or IP of the NFS server |
| **Export Path** | The exact NFS export path (e.g., `/volume1/SentryArchive`) |

No additional SSH setup is required for NFS.

---

## None (No Archiving)

Choose **None** if you don't want to back up clips. Recordings stay on the Pi's SD card (or external drive) and are accessible through the web UI's file browser.

You can always add archiving later by re-running the Setup Wizard.

---

## Archive Options

When any archive method is enabled (not "None"), you can choose which clip types to archive:

| Option | Default | Description |
|--------|---------|-------------|
| **Saved Clips** | On | Manually saved dashcam clips |
| **Sentry Clips** | On | Clips triggered by Sentry Mode events |
| **Recent Clips** | On | Rolling recent footage |
| **Track Mode Clips** | On | Track Mode recordings |