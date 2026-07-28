# Projet — Voiture RC pilotée en 5G (FPV)

Contexte transféré depuis une session de planification claude.ai. Ce fichier
sert de mémoire projet : il capture le matériel, les décisions d'architecture
et les pièges déjà identifiés, ainsi que l'état d'avancement.

## Objectif
Construire une voiture RC pilotée à distance via un lien 5G, avec retour
vidéo FPV (First Person View) faible latence.

## Matériel
- **Calculateur embarqué** : Raspberry Pi 4B
- **Modem 5G** : Waveshare M.2 TO 4G/5G HAT équipé du module **RM500U-CNV**
  (Quectel, plateforme UNISOC/Spreadtrum, 3GPP R15)
- **Contrôle actionneurs** : Arduino Nano (pilote l'ESC + le servo de direction)
- **Caméra** : module CSI (Pi Camera Module 3 ou v2) pour le flux FPV
- **Voiture** : châssis RC avec batterie 2S (7,4 V nominal)
- **Alimentation** : batterie externe dédiée pour la Pi

## Contraintes & pièges déjà identifiés (à ne pas re-découvrir)

### Réseau — le point le plus critique
- **CGNAT** : en 5G/4G grand public, la Pi n'a **pas d'IP publique joignable**.
  Connexion directe impossible. Solutions retenues :
  - VPN mesh **Tailscale** ou **ZeroTier** (WireGuard) — le plus simple ;
  - ou **WebRTC** avec serveur **TURN** (STUN/TURN gère aussi la traversée NAT).
- **Latence** : viser un réalisme ~20-50 ms RTT en 5G FR, avec jitter et pics.
  Pilotable mais pas au niveau d'un lien RC classique.
- **Contrôle en UDP** (ou data channel WebRTC), **jamais en TCP** (le
  head-of-line blocking tue la réactivité sur lien avec perte de paquets).

### Sécurité — failsafe obligatoire
- L'Arduino doit avoir un **watchdog** : si aucune commande valide reçue depuis
  ~200-500 ms → **coupe les gaz et recentre la direction**. Le lien cellulaire
  va tomber, c'est certain ; sans ça une coupure = voiture partie à fond.

### Alimentation
- Le module 5G tire fort en émission (pics de courant). Le port USB de la Pi ne
  suffit pas : mettre l'interrupteur du HAT sur **EXT PWR** et l'alimenter en
  **5V/3A dédié** via son Type-C.
- Sous-tension = resets modem / corruption. Bien dimensionner.

### RM500U-CNV — spécifique module
- **Pas de GNSS/GPS** intégré (module GPS séparé requis si besoin de position).
- **Bandes OK pour la France** : 5G SA n1/n3/n28/n41/n77/n78/n79 (n78 = 3,5 GHz
  principal, n28 = 700 MHz). Aucun souci de couverture opérateur.
- Mode de connexion modem→Pi : préférer **ECM ou NCM** (plus stable que RNDIS
  sous Linux).
- **Tension OS** : le dial-up est instable sous Raspberry Pi OS (Waveshare
  recommande Ubuntu/OpenWrt). MAIS la stack caméra (libcamera + encodeur H.264
  matériel) est mieux rodée sous Raspberry Pi OS. À trancher :
  - soit Ubuntu + vérifier que l'encodeur H.264 matériel (V4L2 M2M) est dispo ;
  - soit Raspberry Pi OS + gérer l'instabilité modem via ECM.

## Bloc en cours : retour vidéo FPV faible latence

### Atout matériel
Le **Pi 4 possède un encodeur H.264 matériel** (VideoCore) — l'utiliser
systématiquement (jamais d'encodage soft). Rester en **H.264** (le H.265 n'est
pas accéléré sur Pi 4). Le Pi 5 a perdu cet encodeur, donc le Pi 4 est un
avantage ici.

### Cibles
- Résolution/fps : **720p@30** ou **848×480@30** (en FPV le fps > résolution).
- Bitrate : **1,5-4 Mbps**, CBR. Contrainte = l'**uplink** cellulaire.
- Encodeur : **profil baseline** (pas de frames B), GOP court / intra ~30,
  `inline-headers`. Réception : jitter buffer minimal.

### Caméra (Module 3)
- **Fixer la mise au point** (`--lens-position`, ~hyperfocale) pour éviter le
  hunting de l'autofocus pendant le pilotage. (Le v2 est à focale fixe.)
- Variante **Wide 120°** idéale pour le FPV si le choix est encore ouvert.
- **Soft-monter** la caméra (rolling shutter + vibrations = effet « jello »).

### Chemin A — MediaMTX + WebRTC (recommandé pour démarrer)
Source `rpiCamera` intégrée à MediaMTX → attaque libcamera, encode en H.264
matériel, sert du WebRTC. Client = **navigateur seul** (rien à installer).

`mediamtx.yml` :
```yaml
paths:
  cam:
    source: rpiCamera
    rpiCameraWidth: 1280
    rpiCameraHeight: 720
    rpiCameraFPS: 30
    rpiCameraBitrate: 3000000
    rpiCameraProfile: baseline
    rpiCameraIDRPeriod: 30
```
Vue : `http://<IP-tailscale-du-pi>:8889/cam`
(vérifier les noms de params `rpiCamera*` dans la doc MediaMTX, ils évoluent.)

### Chemin B — GStreamer RTP brut (pour mesurer le plancher de latence)
Capture + encodage matériel via `rpicam-vid` (Bookworm ; `libcamera-vid` sur
Bullseye), piped vers GStreamer.

Émetteur (Pi) :
```bash
rpicam-vid -t 0 -n --inline \
  --width 1280 --height 720 --framerate 30 \
  --codec h264 --profile baseline --intra 30 --bitrate 3000000 \
  -o - | \
gst-launch-1.0 fdsrc fd=0 ! h264parse ! rtph264pay config-interval=1 pt=96 \
  ! udpsink host=<IP-PC> port=5000 sync=false
```
Récepteur (PC, GStreamer requis) :
```bash
gst-launch-1.0 -v udpsrc port=5000 \
  caps="application/x-rtp,media=video,encoding-name=H264,payload=96" \
  ! rtpjitterbuffer latency=50 ! rtph264depay ! avdec_h264 \
  ! autovideosink sync=false
```
Leviers latence côté réception : `rtpjitterbuffer latency=50` (30 ms si lien
propre, plus si ça saccade) et `sync=false`.

### Démarche de validation (important)
Monter le chemin B **d'abord en LAN/WiFi** (Pi et PC sur la box) pour valider
caméra + encodage matériel + pipeline et mesurer la latence de base « propre ».
**Ensuite** ajouter Tailscale, **puis** la 5G — ainsi quand la latence grimpe on
sait que c'est le réseau, pas le pipeline.

## Blocs suivants (pas encore traités)
- Chaîne de contrôle : Pi ↔ Arduino (série/UART) ↔ ESC + servo direction, avec
  le watchdog failsafe.
- Architecture réseau complète (Tailscale vs WebRTC+TURN, signaling).
- Alimentation détaillée & intégration physique (thermique, montage).
