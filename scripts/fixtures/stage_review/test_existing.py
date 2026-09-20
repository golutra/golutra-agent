import unittest
from inventory import adjust_counts


class Existing(unittest.TestCase):
    def test_add(self):
        self.assertEqual(adjust_counts({"apple": 2}, {"apple": 1}), {"apple": 3})
