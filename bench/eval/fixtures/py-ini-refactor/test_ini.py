import unittest
import loader_a
import loader_b


SAMPLE = """
# top comment
; semicolon comment
[global]
host = example.com
port = 8080 ; inline comment

[api]
base_url = https://${global.host}:${global.port}/v1
health = ${base_url}/health # trailing comment
literal = "a # not a comment ; still value"
"""


class TestIniRefactor(unittest.TestCase):
    def test_shared_core_module_exists(self):
        import ini_core  # noqa: F401
        self.assertTrue(hasattr(ini_core, "parse_ini"))

    def test_both_loaders_identical_and_interpolated(self):
        a = loader_a.load_service_a(SAMPLE)
        b = loader_b.load_service_b(SAMPLE)
        self.assertEqual(a, b)
        self.assertEqual(a["global"]["port"], "8080")
        self.assertEqual(a["api"]["base_url"], "https://example.com:8080/v1")
        self.assertEqual(a["api"]["health"], "https://example.com:8080/v1/health")
        self.assertEqual(a["api"]["literal"], "a # not a comment ; still value")

    def test_cycle_detection(self):
        cyclic = "[s]\na = ${b}\nb = ${a}\n"
        with self.assertRaises(ValueError):
            loader_a.load_service_a(cyclic)

    def test_missing_reference(self):
        missing = "[s]\na = ${nope}\n"
        with self.assertRaises(KeyError):
            loader_b.load_service_b(missing)


if __name__ == "__main__":
    unittest.main()
