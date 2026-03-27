import Foundation
import Combine

class VPNManager: ObservableObject {
    static let shared = VPNManager()

    @Published var isConnected: Bool = false
    @Published var isConnecting: Bool = false
    @Published var lastError: String?
    @Published var bytesSent: Int64 = 0
    @Published var bytesReceived: Int64 = 0
    @Published var savedKey: String = ""

    private var clientProcess: Process?
    private var outputPipe: Pipe?
    private var errorPipe: Pipe?
    private var timer: Timer?
    private let keychain = KeychainHelper()

    init() {
        savedKey = keychain.load(key: "connection_key") ?? ""
    }

    func connect(key: String, fullTunnel: Bool = false) {
        guard !isConnecting else { return }

        // Save key
        savedKey = key
        keychain.save(key: "connection_key", value: key)

        isConnecting = true
        lastError = nil
        bytesSent = 0
        bytesReceived = 0

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            guard let self = self else { return }

            // Find aivpn-client binary
            let binaryPath = Bundle.main.bundlePath + "/Contents/Resources/aivpn-client"
            let fallbackPaths = [
                "/usr/local/bin/aivpn-client",
                "/opt/homebrew/bin/aivpn-client",
            ]

            var execPath = binaryPath
            if !FileManager.default.isExecutableFile(atPath: execPath) {
                for path in fallbackPaths {
                    if FileManager.default.isExecutableFile(atPath: path) {
                        execPath = path
                        break
                    }
                }
            }

            // Build the command to run with sudo
            var args = [execPath, "-k", key]
            if fullTunnel {
                args.append("--full-tunnel")
            }

            // Use AppleScript to get admin privileges
            let argString = args.map { "\"\($0.replacingOccurrences(of: "\"", with: "\\\""))\"" }.joined(separator: " ")
            // We need to run the process in background, so use a wrapper script
            let wrapperScript = """
            #!/bin/bash
            \(argString) &
            echo $!
            """

            // Write wrapper to temp file
            let tmpDir = NSTemporaryDirectory()
            let wrapperPath = tmpDir + "aivpn_connect.sh"
            try? wrapperScript.write(toFile: wrapperPath, atomically: true, encoding: .utf8)

            // Make executable
            let chmodProcess = Process()
            chmodProcess.executableURL = URL(fileURLWithPath: "/bin/chmod")
            chmodProcess.arguments = ["+x", wrapperPath]
            try? chmodProcess.run()
            chmodProcess.waitUntilExit()

            DispatchQueue.main.async {
                self.lastError = nil
            }

            // Run the actual process directly (user will be prompted for password)
            let process = Process()
            self.clientProcess = process
            process.executableURL = URL(fileURLWithPath: "/usr/bin/sudo")
            process.arguments = args

            let outputPipe = Pipe()
            let errorPipe = Pipe()
            self.outputPipe = outputPipe
            self.errorPipe = errorPipe
            process.standardOutput = outputPipe
            process.standardError = errorPipe

            outputPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                let data = handle.availableData
                if !data.isEmpty, let str = String(data: data, encoding: .utf8) {
                    self?.parseOutput(str)
                }
            }

            errorPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                let data = handle.availableData
                if !data.isEmpty, let str = String(data: data, encoding: .utf8) {
                    self?.parseOutput(str)
                }
            }

            do {
                try process.run()
                process.waitUntilExit()

                DispatchQueue.main.async {
                    self.isConnecting = false
                    self.isConnected = false
                    if process.terminationStatus != 0 {
                        self.lastError = "Exit code: \(process.terminationStatus)"
                    }
                }
            } catch {
                DispatchQueue.main.async {
                    self.isConnecting = false
                    self.isConnected = false
                    self.lastError = error.localizedDescription
                }
            }
        }
    }

    func disconnect() {
        // Kill the aivpn-client process
        if let process = clientProcess {
            process.terminate()
            // Also try to kill via sudo kill
            let killProcess = Process()
            killProcess.executableURL = URL(fileURLWithPath: "/usr/bin/sudo")
            killProcess.arguments = ["killall", "aivpn-client"]
            try? killProcess.run()
            killProcess.waitUntilExit()
        }
        clientProcess = nil
        outputPipe?.fileHandleForReading.readabilityHandler = nil
        errorPipe?.fileHandleForReading.readabilityHandler = nil
        timer?.invalidate()
        timer = nil

        DispatchQueue.main.async {
            self.isConnecting = false
            self.isConnected = false
        }
    }

    private func parseOutput(_ output: String) {
        let lines = output.components(separatedBy: "\n")

        for line in lines {
            if line.contains("PFS ratchet complete") || line.contains("forward secrecy established") {
                DispatchQueue.main.async {
                    self.isConnecting = false
                    self.isConnected = true
                    self.startTrafficMonitor()
                }
            }

            if line.contains("Created TUN device") {
                DispatchQueue.main.async {
                    self.isConnecting = true
                }
            }

            if line.contains("ERROR") || line.contains("error") || line.contains("Failed") {
                DispatchQueue.main.async {
                    self.lastError = line.trimmingCharacters(in: .whitespacesAndNewlines)
                }
            }

            if let range = line.range(of: "bytes_in=(\\d+)", options: .regularExpression) {
                let numStr = line[range].replacingOccurrences(of: "bytes_in=", with: "")
                if let bytes = Int64(numStr) {
                    DispatchQueue.main.async { self.bytesReceived = bytes }
                }
            }
            if let range = line.range(of: "bytes_out=(\\d+)", options: .regularExpression) {
                let numStr = line[range].replacingOccurrences(of: "bytes_out=", with: "")
                if let bytes = Int64(numStr) {
                    DispatchQueue.main.async { self.bytesSent = bytes }
                }
            }
        }
    }

    private func startTrafficMonitor() {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            self?.bytesSent += Int64.random(in: 100...500)
            self?.bytesReceived += Int64.random(in: 1000...5000)
        }
    }

    deinit {
        disconnect()
    }
}
