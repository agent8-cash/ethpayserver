import unittest, os, importlib.util

script_path = os.path.join(os.path.dirname(__file__), 'gen-backend-schema.py')
spec = importlib.util.spec_from_file_location('gen_backend_schema', script_path)
gen = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gen)

class TestGenBackendSchema(unittest.TestCase):
    def test_statements(self):
        sql = "SELECT 1; -- comment\n/* block comment */\nSELECT 2;"
        self.assertEqual(gen.statements(sql), ["SELECT 1", "SELECT 2"])
    def test_split_top(self):
        s = "a(b,c),d"
        self.assertEqual(gen.split_top(s), ["a(b,c)", "d"])
    def test_ident(self):
        self.assertEqual(gen.ident('Foo'), 'foo')
        self.assertEqual(gen.ident('"Foo"'), 'Foo')

if __name__ == '__main__':
    unittest.main()
