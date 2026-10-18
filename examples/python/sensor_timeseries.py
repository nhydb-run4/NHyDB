"""Sensor time series with the NumPy batch API.

One record per minute and sensor: writes a day of measurements for three
sensors in one call per sensor, reads a time window back as NumPy columns,
aggregates with NHySQL and checks the database.

Run: python sensor_timeseries.py            (needs: pip install nhydb numpy)
"""
# requires: numpy
import os
import tempfile

import numpy as np

import nhydb

MINUTES = 24 * 60
SENSORS = {1: "roof", 2: "cellar", 3: "garden"}


def main():
    path = os.path.join(tempfile.mkdtemp(), "sensors.nhydb")
    with nhydb.create(path) as db:
        db.create_realm(1, "measurements")
        db.create_shape(1, 1, "sample", [
            {"v": 1, "name": "sensor", "type": "u8"},
            {"v": 2, "name": "minute", "type": "u16"},
            {"v": 3, "name": "temp_c", "type": "f32"},
            {"v": 4, "name": "valid", "type": "bool"},
        ])
        db.create_classic_layout(1, 1, "samples", shape_id=1)

        # record id u = (sensor - 1) * MINUTES + minute + 1  ->  one contiguous block per sensor
        rng = np.random.default_rng(7)
        minute = np.arange(MINUTES, dtype=np.uint16)
        for sensor in SENSORS:
            temp = 12 + 6 * np.sin(minute / MINUTES * 2 * np.pi) + rng.normal(0, 0.3, MINUTES) + sensor
            db.put_numpy_batch(1, (sensor - 1) * MINUTES + 1, {
                "sensor": np.full(MINUTES, sensor, dtype=np.uint8),
                "minute": minute,
                "temp_c": temp.astype(np.float32),
                "valid": rng.random(MINUTES) > 0.01,
            })
        db.commit()

        # one hour of the cellar sensor, 12:00-12:59
        start = (2 - 1) * MINUTES + 12 * 60 + 1
        hour = db.read_numpy(1, start, 60, ["minute", "temp_c", "valid"])
        assert hour["_present"].all() and int(hour["minute"][0]) == 720
        print(f"cellar 12:00-12:59: mean {hour['temp_c'][hour['valid']].mean():.2f} °C")

        db.create_index(1, 1, "by_sensor", "sensor")
        db.build_indexes(commit=True)
        stats = db.sql("SELECT sensor, COUNT(*) AS n, MIN(temp_c) AS lo, MAX(temp_c) AS hi "
                       "FROM samples WHERE valid = ? GROUP BY sensor ORDER BY sensor", [True])
        for row in stats["rows"]:
            print(f"{SENSORS[row['sensor']]:>7}: {row['n']} values, {row['lo']:.1f} … {row['hi']:.1f} °C")
        assert len(stats["rows"]) == 3
        assert db.verify(deep=True)["ok"]


if __name__ == "__main__":
    main()
