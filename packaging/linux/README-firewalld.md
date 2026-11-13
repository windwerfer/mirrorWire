# firewalld service for MirrorWire

`mirrorwire.xml` describes the default presenter port (UDP 47800) so a host
with firewalld can open it as a named service:

```
sudo firewall-cmd --permanent --add-service=mirrorwire
sudo firewall-cmd --reload
```

The file has to live on the *host* at `/etc/firewalld/services/mirrorwire.xml`
or `/usr/lib/firewalld/services/mirrorwire.xml`. The AppImage and Flatpak
bundles include a copy under `share/firewalld/services/` for packagers, but
those paths inside a sandbox are not visible to the host firewalld.

If the service file is not installed, open the port directly:

```
sudo firewall-cmd --add-port=47800/udp
sudo firewall-cmd --permanent --add-port=47800/udp
```

A custom presenter port is not covered by this service file; use
`--add-port=<port>/udp` (or the presenter's Open-port button) instead.
