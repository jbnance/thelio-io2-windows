// lhm-helper — LibreHardwareMonitorLib sidecar for thelio-io2-windows
//
// This small console app wraps the LibreHardwareMonitorLib NuGet package
// to provide direct hardware sensor access without the full LHM GUI.
//
// Protocol (stdin/stdout, line-based JSON):
//   Startup  → stdout: {"status":"ready"}
//   Request  ← stdin:  "read"
//   Response → stdout: {"sensors":[...]}
//   Shutdown ← stdin:  "exit" or EOF
//
// Must be run with administrator privileges for full sensor access.

using System.Text.Json;
using System.Text.Json.Serialization;
using LibreHardwareMonitor.Hardware;

namespace LhmHelper;

/// <summary>
/// A single temperature sensor reading sent to the Rust daemon.
/// </summary>
internal sealed class SensorReading
{
    [JsonPropertyName("id")]
    public string Id { get; set; } = "";

    [JsonPropertyName("name")]
    public string Name { get; set; } = "";

    [JsonPropertyName("value")]
    public double Value { get; set; }

    /// <summary>
    /// Hardware classification: "cpu", "gpu", or "other".
    /// Lets the Rust side classify sensors without parsing IDs.
    /// </summary>
    [JsonPropertyName("hardware")]
    public string Hardware { get; set; } = "other";
}

/// <summary>
/// JSON envelope for a batch of sensor readings.
/// </summary>
internal sealed class SensorResponse
{
    [JsonPropertyName("sensors")]
    public List<SensorReading> Sensors { get; set; } = new();
}

/// <summary>
/// JSON envelope for status messages.
/// </summary>
internal sealed class StatusMessage
{
    [JsonPropertyName("status")]
    public string Status { get; set; } = "";

    [JsonPropertyName("error")]
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Error { get; set; }
}

/// <summary>
/// Visitor that calls Update() on every hardware and sub-hardware node.
/// Required by LibreHardwareMonitorLib to refresh sensor values.
/// </summary>
internal sealed class UpdateVisitor : IVisitor
{
    public void VisitComputer(IComputer computer)
    {
        computer.Traverse(this);
    }

    public void VisitHardware(IHardware hardware)
    {
        hardware.Update();
        foreach (var sub in hardware.SubHardware)
            sub.Accept(this);
    }

    public void VisitSensor(ISensor sensor) { }
    public void VisitParameter(IParameter parameter) { }
}

internal static class Program
{
    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        // Compact output — one JSON object per line
        WriteIndented = false,
    };

    /// <summary>
    /// Classify a hardware type into "cpu", "gpu", or "other".
    /// </summary>
    private static string ClassifyHardware(HardwareType type)
    {
        return type switch
        {
            HardwareType.Cpu => "cpu",
            HardwareType.GpuNvidia => "gpu",
            HardwareType.GpuAmd => "gpu",
            HardwareType.GpuIntel => "gpu",
            _ => "other",
        };
    }

    /// <summary>
    /// Collect all temperature sensors from a hardware node and its sub-hardware.
    /// </summary>
    private static void CollectTemperatures(IHardware hardware, List<SensorReading> readings)
    {
        var hwClass = ClassifyHardware(hardware.HardwareType);

        foreach (var sensor in hardware.Sensors)
        {
            if (sensor.SensorType == SensorType.Temperature && sensor.Value.HasValue)
            {
                readings.Add(new SensorReading
                {
                    Id = sensor.Identifier.ToString(),
                    Name = sensor.Name,
                    Value = Math.Round(sensor.Value.Value, 1),
                    Hardware = hwClass,
                });
            }
        }

        // Sub-hardware (e.g. GPU sub-devices, chipset sensors)
        foreach (var sub in hardware.SubHardware)
        {
            CollectTemperatures(sub, readings);
        }
    }

    /// <summary>
    /// Perform one read cycle: update all hardware and collect temperature sensors.
    /// </summary>
    private static SensorResponse ReadSensors(Computer computer, UpdateVisitor visitor)
    {
        computer.Accept(visitor);

        var response = new SensorResponse();

        foreach (var hardware in computer.Hardware)
        {
            CollectTemperatures(hardware, response.Sensors);
        }

        return response;
    }

    /// <summary>
    /// Write a JSON line to stdout and flush.
    /// </summary>
    private static void WriteJsonLine<T>(T obj)
    {
        var json = JsonSerializer.Serialize(obj, JsonOpts);
        Console.Out.WriteLine(json);
        Console.Out.Flush();
    }

    static int Main()
    {
        Computer? computer = null;

        try
        {
            // Initialize LibreHardwareMonitor with CPU and GPU monitoring.
            computer = new Computer
            {
                IsCpuEnabled = true,
                IsGpuEnabled = true,
                // Motherboard sensors can include chipset temps — useful
                // but not critical. Enable for completeness.
                IsMotherboardEnabled = true,
            };

            computer.Open();

            var visitor = new UpdateVisitor();

            // Signal readiness to the Rust daemon.
            WriteJsonLine(new StatusMessage { Status = "ready" });

            // Enter the request-response loop.
            string? line;
            while ((line = Console.In.ReadLine()) != null)
            {
                var cmd = line.Trim().ToLowerInvariant();

                if (cmd == "exit")
                    break;

                if (cmd == "read")
                {
                    try
                    {
                        var response = ReadSensors(computer, visitor);
                        WriteJsonLine(response);
                    }
                    catch (Exception ex)
                    {
                        WriteJsonLine(new StatusMessage
                        {
                            Status = "error",
                            Error = ex.Message,
                        });
                    }
                }
                // Unknown commands are silently ignored.
            }
        }
        catch (Exception ex)
        {
            // Fatal initialization error — report and exit.
            try
            {
                WriteJsonLine(new StatusMessage
                {
                    Status = "error",
                    Error = $"Initialization failed: {ex.Message}",
                });
            }
            catch
            {
                // If stdout is broken, nothing we can do.
            }
            return 1;
        }
        finally
        {
            computer?.Close();
        }

        return 0;
    }
}
