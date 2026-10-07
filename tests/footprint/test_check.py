import copy
import unittest
from check import compare


class ComparisonTests(unittest.TestCase):
    def setUp(self):
        self.baseline = {'rustc': 'rustc test', 'target': 'x86_64-linux',
                         'profiles': {'minimal': {'anvil_bytes': 4000000, 'dependencies': ['one 1.0']}}}

    def test_small_variation_and_shrinkage_do_not_warn(self):
        for size in (3500000, 4100000, 4480000):
            current = copy.deepcopy(self.baseline)
            current['profiles']['minimal']['anvil_bytes'] = size
            self.assertEqual(compare(self.baseline, current), [])

    def test_material_growth_is_visible(self):
        current = copy.deepcopy(self.baseline)
        current['profiles']['minimal']['anvil_bytes'] = 4500000
        self.assertIn('binary grew', compare(self.baseline, current)[0])

    def test_changed_compiler_does_not_report_a_false_size_regression(self):
        current = copy.deepcopy(self.baseline)
        current['rustc'] = 'different compiler'
        current['profiles']['minimal']['anvil_bytes'] = 5000000
        notices = compare(self.baseline, current)
        self.assertEqual(len(notices), 1)
        self.assertIn('informational', notices[0])

    def test_dependency_replacement_is_always_reported(self):
        current = copy.deepcopy(self.baseline)
        current['profiles']['minimal']['dependencies'] = ['one 2.0']
        notices = compare(self.baseline, current)
        self.assertIn('one 2.0', notices[0])
        self.assertIn('one 1.0', notices[0])


if __name__ == '__main__':
    unittest.main()
