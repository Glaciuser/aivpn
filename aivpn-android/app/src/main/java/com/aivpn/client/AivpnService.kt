package com.aivpn.client

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.VpnService
import android.os.ParcelFileDescriptor
import android.util.Log
import kotlinx.coroutines.*
import kotlinx.coroutines.selects.select
import java.io.FileInputStream
import java.io.FileOutputStream
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.net.SocketTimeoutException

/**
 * Android VPN service — simplified WireGuard-style architecture.
 *
 * Model:
 * 1) One UDP socket
 * 2) One TUN interface
 * 3) Three loops (tun->udp, udp->tun, keepalive)
 * 4) Any loop failure tears down all resources and reconnects
 */
class AivpnService : VpnService() {

    companion object {
        const val ACTION_CONNECT = "com.aivpn.CONNECT"
        const val ACTION_DISCONNECT = "com.aivpn.DISCONNECT"
        private const val CHANNEL_ID = "aivpn_vpn"
        private const val NOTIFICATION_ID = 1
        private const val TUN_MTU = 1420
        private const val KEEPALIVE_INTERVAL_MS = 25_000L   // 25 s: balanced NAT keep-alive vs battery
        private const val HANDSHAKE_TIMEOUT_MS = 10_000L
        private const val SOCKET_TIMEOUT_MS = 5_000L
        private const val DEAD_TUNNEL_TIMEOUT_MS = 60_000L   // 60 s: tolerates Doze-mode / LTE gaps
        private const val INITIAL_RETRY_DELAY_MS = 500L
        private const val MAX_RETRY_DELAY_MS = 8_000L
        private const val REKEY_AFTER_TIME_MS = 1_800_000L  // 30 min: avoid reconnect every 3 min
        private const val TAG = "AivpnService"

        @Volatile var statusCallback: ((connected: Boolean, status: String) -> Unit)? = null
        @Volatile var trafficCallback: ((uploadBytes: Long, downloadBytes: Long) -> Unit)? = null
        @Volatile var isRunning = false
        @Volatile var lastStatusText: String = ""
    }

    // Tunnel resources
    private var vpnInterface: ParcelFileDescriptor? = null
    private var udpSocket: DatagramSocket? = null
    private var tunIn: FileInputStream? = null
    private var tunOut: FileOutputStream? = null

    // Coroutine lifecycle
    private var serviceJob: Job? = null
    private val serviceScope = CoroutineScope(Dispatchers.IO + SupervisorJob())
    @Volatile private var manualDisconnect = false

    // Saved params for reconnect
    @Volatile private var savedServerAddr: String? = null
    @Volatile private var savedServerKey: String? = null
    @Volatile private var savedPsk: String? = null
    @Volatile private var savedVpnIp: String? = null
    @Volatile private var savedVpnIp6: String? = null

    // Traffic counters
    @Volatile private var totalUploadBytes: Long = 0
    @Volatile private var totalDownloadBytes: Long = 0

    // Outbound activity tracking for persistent keepalive
    @Volatile private var lastSendTime = 0L

    // Dead tunnel detection
    @Volatile private var lastReceiveTime = 0L

    // Whether the current session made it past handshake
    @Volatile private var sessionEstablished = false

    @Volatile private var sessionNetwork: Network? = null

    // Network change detection: closing UDP socket signals active tunnel to reconnect
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> {
                val server = intent.getStringExtra("server") ?: return START_NOT_STICKY
                val serverKey = intent.getStringExtra("server_key") ?: return START_NOT_STICKY
                val pskBase64 = intent.getStringExtra("psk")
                val vpnIp  = intent.getStringExtra("vpn_ip")
                val vpnIp6 = intent.getStringExtra("vpn_ip6")
                startVpn(server, serverKey, pskBase64, vpnIp, vpnIp6)
            }
            ACTION_DISCONNECT -> {
                stopVpn()
            }
        }
        return START_STICKY
    }

    private fun startVpn(
        serverAddr: String,
        serverKeyBase64: String,
        pskBase64: String? = null,
        vpnIp: String? = null,
        vpnIp6: String? = null
    ) {
        Log.d(TAG, "startVpn: server=$serverAddr")

        savedServerAddr = serverAddr
        savedServerKey = serverKeyBase64
        savedPsk = pskBase64
        savedVpnIp  = vpnIp
        savedVpnIp6 = vpnIp6

        manualDisconnect = false
        serviceJob?.cancel()
        serviceJob = null
        closeTunnel()

        createNotificationChannel()
        startForeground(NOTIFICATION_ID, buildNotification(getString(R.string.notification_connecting)))

        totalUploadBytes = 0
        totalDownloadBytes = 0
        lastSendTime = 0L
        sessionEstablished = false

        unregisterNetworkCallback()
        registerNetworkCallback()

        val job = serviceScope.launch {
            var retryDelayMs = INITIAL_RETRY_DELAY_MS
            try {
                while (isActive && !manualDisconnect) {
                    try {
                        sessionEstablished = false
                        runTunnel()
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Log.e(TAG, "Tunnel error: ${e.message}", e)
                        coroutineContext.cancelChildren()
                        closeTunnel()

                        if (manualDisconnect) break

                        if (sessionEstablished) {
                            retryDelayMs = INITIAL_RETRY_DELAY_MS
                        }

                        statusCallback?.invoke(true, getString(R.string.status_reconnecting))
                        updateNotification(getString(R.string.notification_connecting))
                        Log.d(TAG, "Reconnecting in ${retryDelayMs}ms")
                        delay(retryDelayMs)
                        retryDelayMs = (retryDelayMs * 2).coerceAtMost(MAX_RETRY_DELAY_MS)
                    }
                }
            } catch (e: CancellationException) {
                Log.d(TAG, "Service cancelled")
            }
            finally {
                if (serviceJob === coroutineContext[Job]) {
                    isRunning = false
                    closeTunnel()
                    serviceJob = null
                    if (!manualDisconnect) {
                        stopForeground(STOP_FOREGROUND_REMOVE)
                        stopSelf()
                    }
                }
            }
        }
        serviceJob = job
    }

    /**
     * One tunnel session: handshake -> TUN setup -> forwarding loops.
     * Any exception means session is dead and caller will reconnect.
     */
    private suspend fun runTunnel() {
        val network = waitForNetwork()
        sessionNetwork = network

        val (host, port) = parseServerAddr(
            savedServerAddr ?: throw Exception("No server address")
        )

        val serverKeyBase64 = savedServerKey ?: throw Exception("No server key")
        val serverKey = android.util.Base64.decode(serverKeyBase64, android.util.Base64.DEFAULT)
        if (serverKey.size != 32) throw Exception("Invalid server key size: ${serverKey.size}")

        val psk: ByteArray? = savedPsk?.let {
            val decoded = android.util.Base64.decode(it, android.util.Base64.DEFAULT)
            if (decoded.size == 32) decoded else null
        }

        val crypto = AivpnCrypto(serverKey, psk)

        Log.d(TAG, "Creating UDP socket to $host:$port")
        val socket = DatagramSocket()
        if (!protect(socket)) {
            socket.close()
            throw RuntimeException("protect() failed")
        }

        network.bindSocket(socket)
        socket.connect(InetSocketAddress(host, port))
        socket.soTimeout = HANDSHAKE_TIMEOUT_MS.toInt()
        udpSocket = socket

        Log.d(TAG, "Sending init handshake packet")
        val initPacket = crypto.buildInitPacket()
        socket.send(DatagramPacket(initPacket, initPacket.size))
        lastSendTime = System.currentTimeMillis()

        Log.d(TAG, "Waiting for ServerHello response")
        val recvBuf = ByteArray(2048)
        val response = DatagramPacket(recvBuf, recvBuf.size)
        try {
            socket.receive(response)
        } catch (e: SocketTimeoutException) {
            throw RuntimeException("Handshake timeout (${HANDSHAKE_TIMEOUT_MS}ms)", e)
        }
        Log.d(TAG, "ServerHello received, length=${response.length}")

        val serverHelloData = recvBuf.copyOf(response.length)
        if (!crypto.processServerHello(serverHelloData)) {
            throw RuntimeException("Handshake failed (ServerHello validation)")
        }
        Log.d(TAG, "Handshake successful")
        socket.soTimeout = SOCKET_TIMEOUT_MS.toInt()
        sessionEstablished = true
        lastSendTime = System.currentTimeMillis()
        lastReceiveTime = System.currentTimeMillis()

        val tunAddress4 = savedVpnIp ?: "10.0.0.2"
        Log.d(TAG, "Establishing TUN interface: IPv4=$tunAddress4")
        val builder = Builder()
            .setSession("AIVPN")
            // IPv4 only — server does not yet support IPv6 tunnelling;
            // addRoute("::", 0) causes Android to prefer IPv6, route all its traffic
            // into the tunnel where packets are dropped → continuous IPv4/IPv6 oscillation.
            .addAddress(tunAddress4, 24)
            .addRoute("0.0.0.0", 0)
            .addDnsServer("8.8.8.8")
            .addDnsServer("1.1.1.1")
            .setMtu(TUN_MTU)
            .setBlocking(true)

        vpnInterface = builder.establish() ?: throw Exception("Failed to establish VPN interface")
        // Bind VPN to the physical network so traffic routes correctly after network switches
        setUnderlyingNetworks(arrayOf(network))
        Log.d(TAG, "TUN interface established")

        val localTunIn = FileInputStream(vpnInterface!!.fileDescriptor)
        val localTunOut = FileOutputStream(vpnInterface!!.fileDescriptor)
        tunIn = localTunIn
        tunOut = localTunOut

        isRunning = true
        lastStatusText = getString(R.string.status_connected, host)
        statusCallback?.invoke(true, lastStatusText)
        updateNotification(getString(R.string.notification_connected, host))

        coroutineScope {
            val tunToUdp = launch { tunToServer(localTunIn, socket, crypto) }
            val udpToTun = launch { serverToTun(socket, localTunOut, crypto) }
            val keepaliveLoop = launch { keepaliveToServer(socket, crypto) }
            val rekeyLoop = launch { rekeyTimer() }

            select<Unit> {
                tunToUdp.onJoin { }
                udpToTun.onJoin { }
                keepaliveLoop.onJoin { }
                rekeyLoop.onJoin { }
            }

            // 1. Cancel children so isActive becomes false in all child coroutines.
            currentCoroutineContext().cancelChildren()
            // 2. Close blocking I/O AFTER cancellation signal is sent.
            //    Blocked tunIn.read() / socket.receive() throw IOException;
            //    children suppress it because isActive is already false.
            try { localTunIn.close() } catch (_: Exception) {}
            try { socket.close() } catch (_: Exception) {}
        }

        throw RuntimeException("Tunnel forwarding stopped")
    }

    /**
     * TUN → Server: read IP packets from TUN, encrypt and send as UDP.
     */
    private suspend fun tunToServer(
        tunIn: FileInputStream,
        socket: DatagramSocket,
        crypto: AivpnCrypto
    ) = withContext(Dispatchers.IO) {
        val buf = ByteArray(TUN_MTU + 100)
        while (isActive) {
            try {
                val n = tunIn.read(buf)
                if (n <= 0) {
                    if (n < 0) throw RuntimeException("TUN closed")
                    continue
                }
                val encrypted = crypto.encryptDataPacket(buf.copyOf(n))
                socket.send(DatagramPacket(encrypted, encrypted.size))
                lastSendTime = System.currentTimeMillis()
                totalUploadBytes += n
                trafficCallback?.invoke(totalUploadBytes, totalDownloadBytes)
            } catch (e: Exception) {
                if (isActive) throw e
            }
        }
    }

    /**
     * Server → TUN: receive/decrypt UDP packets and write IP payload to TUN.
     */
    private suspend fun serverToTun(
        socket: DatagramSocket,
        tunOut: FileOutputStream,
        crypto: AivpnCrypto
    ) = withContext(Dispatchers.IO) {
        val buf = ByteArray(TUN_MTU + 200)
        while (isActive) {
            try {
                val pkt = DatagramPacket(buf, buf.size)
                socket.receive(pkt)
                lastReceiveTime = System.currentTimeMillis()

                val decrypted = crypto.decryptDataPacket(buf.copyOf(pkt.length))
                if (decrypted != null && decrypted.isNotEmpty()) {
                    tunOut.write(decrypted)
                    totalDownloadBytes += decrypted.size
                    trafficCallback?.invoke(totalUploadBytes, totalDownloadBytes)
                }
            } catch (e: SocketTimeoutException) {
                val silence = System.currentTimeMillis() - lastReceiveTime
                if (silence > DEAD_TUNNEL_TIMEOUT_MS) {
                    throw RuntimeException("Dead tunnel")
                }
            } catch (e: Exception) {
                if (isActive) throw e
            }
        }
    }

    /**
     * Keep NAT mapping alive; does not decide liveness.
     */
    private suspend fun keepaliveToServer(
        socket: DatagramSocket,
        crypto: AivpnCrypto
    ) = withContext(Dispatchers.IO) {
        while (isActive) {
            try {
                delay(KEEPALIVE_INTERVAL_MS)

                val idleMs = System.currentTimeMillis() - lastSendTime
                if (idleMs < KEEPALIVE_INTERVAL_MS) {
                    continue
                }

                val keepalive = crypto.buildKeepalivePacket()
                socket.send(DatagramPacket(keepalive, keepalive.size))
                lastSendTime = System.currentTimeMillis()
            } catch (e: Exception) {
                if (isActive) throw e
            }
        }
    }

    /**
     * Triggers a re-handshake for forward secrecy after REKEY_AFTER_TIME_MS.
     * Completes normally; the outer select() tears down and reconnects.
     */
    private suspend fun rekeyTimer() {
        delay(REKEY_AFTER_TIME_MS)
        Log.d(TAG, "Rekey interval elapsed — initiating fresh handshake")
    }

    private fun registerNetworkCallback() {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

        // Use registerNetworkCallback with NET_CAPABILITY_NOT_VPN instead of
        // registerDefaultNetworkCallback.
        //
        // registerDefaultNetworkCallback fires onLost(physical) every time VPN establish()
        // takes over as the default route, and onAvailable(physical) every time VPN closes —
        // causing spurious reconnects even when the physical network is perfectly alive.
        //
        // With NOT_VPN the system filters out VPN networks entirely: onAvailable/onLost
        // are delivered only for real physical networks (WiFi, LTE, etc.), so:
        //   • VPN establish()  → no callback  ✅
        //   • VPN teardown    → no callback  ✅
        //   • WiFi → LTE      → onLost(wifi) + onAvailable(lte)  ✅
        //   • Physical gone   → onLost(physical)  ✅
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()

        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                val session = sessionNetwork ?: return
                if (network == session) return
                val newCaps = cm.getNetworkCapabilities(network) ?: return
                val sessionIsWifi = cm.getNetworkCapabilities(session)
                    ?.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) == true

                if (newCaps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
                    Log.d(TAG, "Upgrading to WiFi: $session -> $network")
                    setUnderlyingNetworks(arrayOf(network))
                    udpSocket?.close()
                    return
                }

                if (!sessionIsWifi) {
                    Log.d(TAG, "Switching network: $session -> $network")
                    setUnderlyingNetworks(arrayOf(network))
                    udpSocket?.close()
                }
            }

            override fun onLost(network: Network) {
                if (network != sessionNetwork) return
                Log.d(TAG, "Session network lost: $network")
                udpSocket?.close()
            }
        }
        try {
            cm.registerNetworkCallback(request, callback)
            networkCallback = callback
        } catch (e: Exception) {
            Log.e(TAG, "Failed to register NetworkCallback: ${e.message}", e)
        }
    }

    private fun unregisterNetworkCallback() {
        networkCallback?.let {
            try {
                (getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager)
                    .unregisterNetworkCallback(it)
            } catch (_: Exception) {}
            networkCallback = null
        }
    }

    private fun stopVpn() {
        manualDisconnect = true
        unregisterNetworkCallback()
        serviceJob?.cancel()
        serviceJob = null
        closeTunnel()
        isRunning = false
        lastStatusText = getString(R.string.status_disconnected)
        statusCallback?.invoke(false, lastStatusText)
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    private fun isVpnNetwork(network: Network): Boolean {
        return (getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager)
            .getNetworkCapabilities(network)
            ?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true
    }

    private suspend fun waitForNetwork(): Network {
        val cm = getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        while (currentCoroutineContext().isActive) {
            val network = cm.activeNetwork
            if (network != null) {
                val caps = cm.getNetworkCapabilities(network)
                if (caps != null && !caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                    return network
                }
            }
            delay(300L)
        }
        throw CancellationException("Cancelled while waiting for network")
    }

    /**
     * Parses "host:port", "[ipv6]:port", or bare "host" → Pair(host, port).
     * split(":") breaks on every colon and crashes on IPv6 addresses.
     */
    private fun parseServerAddr(serverAddr: String): Pair<String, Int> {
        // IPv6 bracket notation: [2001:db8::1]:443
        if (serverAddr.startsWith("[")) {
            val bracket = serverAddr.indexOf(']')
            if (bracket > 0) {
                val host = serverAddr.substring(1, bracket)
                val port = if (bracket + 1 < serverAddr.length && serverAddr[bracket + 1] == ':')
                    serverAddr.substring(bracket + 2).toIntOrNull() ?: 443
                else 443
                return Pair(host, port)
            }
        }
        // IPv4 / hostname: "1.2.3.4:443" or bare "1.2.3.4"
        val lastColon = serverAddr.lastIndexOf(':')
        val port = if (lastColon >= 0) serverAddr.substring(lastColon + 1).toIntOrNull() else null
        return if (port != null)
            Pair(serverAddr.substring(0, lastColon), port)
        else
            Pair(serverAddr, 443)
    }

    private fun closeTunnel() {
        sessionNetwork = null
        try { vpnInterface?.close() } catch (_: Exception) {}
        try { tunIn?.close() } catch (_: Exception) {}
        try { tunOut?.close() } catch (_: Exception) {}
        try { udpSocket?.close() } catch (_: Exception) {}

        tunIn = null
        tunOut = null
        vpnInterface = null
        udpSocket = null
    }

    override fun onDestroy() {
        manualDisconnect = true
        unregisterNetworkCallback()
        serviceJob?.cancel()
        serviceJob = null
        closeTunnel()
        isRunning = false
        serviceScope.cancel()          // release the SupervisorJob and all children
        super.onDestroy()
    }

    // ──────────── Notifications ────────────

    private fun createNotificationChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID, getString(R.string.notification_channel),
            NotificationManager.IMPORTANCE_LOW
        ).apply {
            description = getString(R.string.notification_channel_desc)
        }
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(channel)
    }

    private fun buildNotification(text: String): Notification {
        val pendingIntent = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("AIVPN")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.ic_lock_lock)
            .setContentIntent(pendingIntent)
            .setOngoing(true)
            .build()
    }

    private fun updateNotification(text: String) {
        val nm = getSystemService(NotificationManager::class.java)
        nm.notify(NOTIFICATION_ID, buildNotification(text))
    }
}
